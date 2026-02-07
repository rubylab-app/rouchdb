//! Integration tests against a real CouchDB instance.
//!
//! These tests require a running CouchDB:
//!   docker compose up -d
//!
//! Run with:
//!   cargo test -p rouchdb --test couchdb_integration -- --ignored
//!
//! All tests are marked `#[ignore]` so they don't run in `cargo test`.

use std::collections::HashMap;

use rouchdb::{
    AllDocsOptions, ChangesOptions, Database,
    FindOptions, GetAttachmentOptions, GetOptions, ReplicationOptions, RouchError,
    SortField, query_view, ReduceFn, ViewQueryOptions,
};

/// CouchDB URL. Override with COUCHDB_URL env var.
/// Default matches the docker-compose.yml credentials.
fn couchdb_url() -> String {
    std::env::var("COUCHDB_URL")
        .unwrap_or_else(|_| "http://admin:password@localhost:15984".to_string())
}

/// Create a fresh CouchDB database with a unique name, returning its URL.
async fn fresh_remote_db(prefix: &str) -> String {
    let db_name = format!("{}_{}", prefix, uuid::Uuid::new_v4().to_string().replace('-', ""));
    let url = format!("{}/{}", couchdb_url(), db_name);

    // Create the database
    let client = reqwest::Client::new();
    let resp = client.put(&url).send().await.unwrap();
    assert!(
        resp.status().is_success(),
        "Failed to create DB {}: {}",
        db_name,
        resp.status()
    );

    url
}

/// Delete a CouchDB database.
async fn delete_remote_db(url: &str) {
    let client = reqwest::Client::new();
    let _ = client.delete(url).send().await;
}

// ==========================================================================
// 1. Basic CRUD via HTTP adapter
// ==========================================================================

#[tokio::test]
#[ignore]
async fn http_put_and_get() {
    let url = fresh_remote_db("http_crud").await;
    let db = Database::http(&url);

    let result = db.put("doc1", serde_json::json!({"name": "Alice"})).await.unwrap();
    assert!(result.ok);

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["name"], "Alice");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn http_update_document() {
    let url = fresh_remote_db("http_update").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev = r1.rev.unwrap();

    let r2 = db.update("doc1", &rev, serde_json::json!({"v": 2})).await.unwrap();
    assert!(r2.ok);

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn http_delete_document() {
    let url = fresh_remote_db("http_delete").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev = r1.rev.unwrap();

    let r2 = db.remove("doc1", &rev).await.unwrap();
    assert!(r2.ok);

    let err = db.get("doc1").await;
    assert!(err.is_err());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn http_all_docs() {
    let url = fresh_remote_db("http_alldocs").await;
    let db = Database::http(&url);

    db.put("alice", serde_json::json!({"name": "Alice"})).await.unwrap();
    db.put("bob", serde_json::json!({"name": "Bob"})).await.unwrap();
    db.put("charlie", serde_json::json!({"name": "Charlie"})).await.unwrap();

    let result = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(result.total_rows, 3);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn http_changes_feed() {
    let url = fresh_remote_db("http_changes").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();

    let changes = db
        .changes(ChangesOptions::default())
        .await
        .unwrap();
    assert_eq!(changes.results.len(), 2);

    delete_remote_db(&url).await;
}

// ==========================================================================
// 2. Basic replication (local ↔ remote)
// ==========================================================================

#[tokio::test]
#[ignore]
async fn replicate_memory_to_couchdb() {
    let url = fresh_remote_db("repl_to_couch").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local.put("doc1", serde_json::json!({"name": "Alice"})).await.unwrap();
    local.put("doc2", serde_json::json!({"name": "Bob"})).await.unwrap();
    local.put("doc3", serde_json::json!({"name": "Charlie"})).await.unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 3);

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["name"], "Alice");

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 3);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_couchdb_to_memory() {
    let url = fresh_remote_db("repl_from_couch").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("doc1", serde_json::json!({"city": "NYC"})).await.unwrap();
    remote.put("doc2", serde_json::json!({"city": "LA"})).await.unwrap();

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 2);

    let doc = local.get("doc1").await.unwrap();
    assert_eq!(doc.data["city"], "NYC");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn bidirectional_sync_with_couchdb() {
    let url = fresh_remote_db("bidir_sync").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local.put("local_doc", serde_json::json!({"from": "local"})).await.unwrap();
    remote.put("remote_doc", serde_json::json!({"from": "remote"})).await.unwrap();

    let (push, pull) = local.sync(&remote).await.unwrap();
    assert!(push.ok);
    assert!(pull.ok);

    let _ = local.get("remote_doc").await.unwrap();
    let _ = remote.get("local_doc").await.unwrap();

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn incremental_replication_to_couchdb() {
    let url = fresh_remote_db("incr_repl").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let r1 = local.replicate_to(&remote).await.unwrap();
    assert_eq!(r1.docs_written, 1);

    local.put("doc2", serde_json::json!({"v": 2})).await.unwrap();
    local.put("doc3", serde_json::json!({"v": 3})).await.unwrap();
    let r2 = local.replicate_to(&remote).await.unwrap();
    assert_eq!(r2.docs_read, 2);
    assert_eq!(r2.docs_written, 2);

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 3);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_deletes_to_couchdb() {
    let url = fresh_remote_db("repl_del").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    local.remove("doc1", &r1.rev.unwrap()).await.unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);

    let err = remote.get("doc1").await;
    assert!(err.is_err());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_updates_to_couchdb() {
    let url = fresh_remote_db("repl_upd").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    local
        .update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();

    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn batched_replication_to_couchdb() {
    let url = fresh_remote_db("batch_repl").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    for i in 0..25 {
        local
            .put(
                &format!("doc{:03}", i),
                serde_json::json!({"i": i}),
            )
            .await
            .unwrap();
    }

    let result = local
        .replicate_to_with_opts(
            &remote,
            ReplicationOptions {
                batch_size: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 25);

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 25);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn already_synced_noop() {
    let url = fresh_remote_db("synced_noop").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 0);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_memory_to_redb() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let memory = Database::memory("source");
    let redb = Database::open(&path, "target").unwrap();

    memory.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    memory.put("doc2", serde_json::json!({"v": 2})).await.unwrap();

    let result = memory.replicate_to(&redb).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 2);

    let doc = redb.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 1);
}

#[tokio::test]
#[ignore]
async fn replicate_redb_to_couchdb() {
    let url = fresh_remote_db("redb_to_couch").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let local = Database::open(&path, "local").unwrap();
    let remote = Database::http(&url);

    local.put("doc1", serde_json::json!({"origin": "redb"})).await.unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 1);

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["origin"], "redb");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_query_against_couchdb_data() {
    let url = fresh_remote_db("mango_couch").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("alice", serde_json::json!({"name": "Alice", "age": 30})).await.unwrap();
    remote.put("bob", serde_json::json!({"name": "Bob", "age": 25})).await.unwrap();
    remote.put("charlie", serde_json::json!({"name": "Charlie", "age": 35})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local
        .find(FindOptions {
            selector: serde_json::json!({"age": {"$gte": 30}}),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(result.docs.len(), 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn multiple_sync_rounds() {
    let url = fresh_remote_db("multi_sync").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Round 1: local creates, syncs
    local.put("doc1", serde_json::json!({"round": 1})).await.unwrap();
    local.sync(&remote).await.unwrap();

    // Round 2: remote creates, syncs
    remote.put("doc2", serde_json::json!({"round": 2})).await.unwrap();
    local.sync(&remote).await.unwrap();

    // Round 3: both create, sync
    local.put("doc3", serde_json::json!({"round": 3})).await.unwrap();
    remote.put("doc4", serde_json::json!({"round": 4})).await.unwrap();
    local.sync(&remote).await.unwrap();

    let local_info = local.info().await.unwrap();
    let remote_info = remote.info().await.unwrap();
    assert_eq!(local_info.doc_count, 4);
    assert_eq!(remote_info.doc_count, 4);

    delete_remote_db(&url).await;
}

// ==========================================================================
// 3. Document data diversity — roundtrip through CouchDB
// ==========================================================================

#[tokio::test]
#[ignore]
async fn data_nested_objects_roundtrip() {
    let url = fresh_remote_db("data_nested").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let data = serde_json::json!({
        "address": {
            "street": "123 Main St",
            "city": "New York",
            "geo": {
                "lat": 40.7128,
                "lng": -74.0060
            }
        },
        "contacts": {
            "email": "alice@example.com",
            "phones": {
                "home": "555-0100",
                "work": "555-0200"
            }
        }
    });

    local.put("doc1", data.clone()).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["address"]["city"], "New York");
    assert_eq!(doc.data["address"]["geo"]["lat"], 40.7128);
    assert_eq!(doc.data["contacts"]["phones"]["work"], "555-0200");

    // Pull back and verify
    let local2 = Database::memory("local2");
    local2.replicate_from(&remote).await.unwrap();
    let doc2 = local2.get("doc1").await.unwrap();
    assert_eq!(doc2.data["address"]["geo"]["lng"], -74.006);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn data_arrays_roundtrip() {
    let url = fresh_remote_db("data_arrays").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let data = serde_json::json!({
        "tags": ["rust", "database", "sync"],
        "matrix": [[1, 2, 3], [4, 5, 6]],
        "nested": [{"name": "a"}, {"name": "b"}]
    });

    local.put("doc1", data).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["tags"][0], "rust");
    assert_eq!(doc.data["tags"][2], "sync");
    assert_eq!(doc.data["matrix"][1][2], 6);
    assert_eq!(doc.data["nested"][0]["name"], "a");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn data_null_and_bool_roundtrip() {
    let url = fresh_remote_db("data_nullbool").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let data = serde_json::json!({
        "optional": null,
        "nested_null": {"inner": null},
        "active": true,
        "deleted": false,
        "flags": [true, false, null]
    });

    local.put("doc1", data).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert!(doc.data["optional"].is_null());
    assert!(doc.data["nested_null"]["inner"].is_null());
    assert_eq!(doc.data["active"], true);
    assert_eq!(doc.data["deleted"], false);
    assert!(doc.data["flags"][2].is_null());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn data_numeric_types_roundtrip() {
    let url = fresh_remote_db("data_nums").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let data = serde_json::json!({
        "integer": 42,
        "negative": -7,
        "zero": 0,
        "float": 3.14159,
        "small_float": 0.001,
        "negative_float": -273.15,
        "big": 9999999999_i64
    });

    local.put("doc1", data).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["integer"], 42);
    assert_eq!(doc.data["negative"], -7);
    assert_eq!(doc.data["zero"], 0);
    assert!((doc.data["float"].as_f64().unwrap() - 3.14159).abs() < 1e-10);
    assert_eq!(doc.data["small_float"], 0.001);
    assert_eq!(doc.data["negative_float"], -273.15);
    assert_eq!(doc.data["big"], 9999999999_i64);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn data_empty_structures_roundtrip() {
    let url = fresh_remote_db("data_empty").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let data = serde_json::json!({
        "empty_arr": [],
        "empty_obj": {},
        "empty_str": "",
        "nested_empty": {"a": [], "b": {}}
    });

    local.put("doc1", data).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["empty_arr"].as_array().unwrap().len(), 0);
    assert_eq!(doc.data["empty_obj"].as_object().unwrap().len(), 0);
    assert_eq!(doc.data["empty_str"], "");
    assert_eq!(doc.data["nested_empty"]["a"].as_array().unwrap().len(), 0);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn data_mixed_type_array_roundtrip() {
    let url = fresh_remote_db("data_mixed").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let data = serde_json::json!({
        "mix": [1, "two", true, null, {"nested": 5}, [6, 7]]
    });

    local.put("doc1", data).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    let mix = doc.data["mix"].as_array().unwrap();
    assert_eq!(mix[0], 1);
    assert_eq!(mix[1], "two");
    assert_eq!(mix[2], true);
    assert!(mix[3].is_null());
    assert_eq!(mix[4]["nested"], 5);
    assert_eq!(mix[5][1], 7);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn data_unicode_roundtrip() {
    let url = fresh_remote_db("data_unicode").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let data = serde_json::json!({
        "emoji": "\u{1F980}\u{1F389}",
        "japanese": "\u{6771}\u{4EAC}",
        "chinese": "\u{4F60}\u{597D}\u{4E16}\u{754C}",
        "korean": "\u{C548}\u{B155}\u{D558}\u{C138}\u{C694}",
        "arabic": "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}",
        "accented": "caf\u{00E9} na\u{00EF}ve r\u{00E9}sum\u{00E9}",
        "special_chars": "line1\nline2\ttab\\backslash"
    });

    local.put("doc1", data).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["emoji"], "\u{1F980}\u{1F389}");
    assert_eq!(doc.data["japanese"], "\u{6771}\u{4EAC}");
    assert_eq!(doc.data["accented"], "caf\u{00E9} na\u{00EF}ve r\u{00E9}sum\u{00E9}");
    assert_eq!(doc.data["special_chars"], "line1\nline2\ttab\\backslash");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn data_large_document() {
    let url = fresh_remote_db("data_large").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Build a doc with 100 fields and nested structure
    let mut obj = serde_json::Map::new();
    for i in 0..100 {
        obj.insert(format!("field_{}", i), serde_json::json!({
            "index": i,
            "value": format!("value_{}", i),
            "nested": {"depth": 1, "data": [i, i*2, i*3]}
        }));
    }
    let data = serde_json::Value::Object(obj);

    local.put("big_doc", data).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("big_doc").await.unwrap();
    assert_eq!(doc.data["field_0"]["index"], 0);
    assert_eq!(doc.data["field_99"]["value"], "value_99");
    assert_eq!(doc.data["field_50"]["nested"]["data"][2], 150);

    delete_remote_db(&url).await;
}

// ==========================================================================
// 4. Special document IDs
// ==========================================================================

#[tokio::test]
#[ignore]
async fn special_id_with_spaces() {
    let url = fresh_remote_db("id_spaces").await;
    let db = Database::http(&url);

    db.put("my document", serde_json::json!({"v": 1})).await.unwrap();
    let doc = db.get("my document").await.unwrap();
    assert_eq!(doc.data["v"], 1);
    assert_eq!(doc.id, "my document");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn special_id_with_unicode() {
    let url = fresh_remote_db("id_unicode").await;
    let db = Database::http(&url);

    db.put("doc_\u{00E9}\u{00E8}\u{00EA}", serde_json::json!({"v": 1})).await.unwrap();
    let doc = db.get("doc_\u{00E9}\u{00E8}\u{00EA}").await.unwrap();
    assert_eq!(doc.data["v"], 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn special_id_replicate_roundtrip() {
    let url = fresh_remote_db("id_repl").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Various special IDs
    local.put("has spaces", serde_json::json!({"t": "spaces"})).await.unwrap();
    local.put("has/slash", serde_json::json!({"t": "slash"})).await.unwrap();
    local.put("has+plus", serde_json::json!({"t": "plus"})).await.unwrap();
    local.put("has?question", serde_json::json!({"t": "question"})).await.unwrap();

    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("has spaces").await.unwrap();
    assert_eq!(doc.data["t"], "spaces");

    let doc = remote.get("has+plus").await.unwrap();
    assert_eq!(doc.data["t"], "plus");

    delete_remote_db(&url).await;
}

// ==========================================================================
// 5. Error conditions
// ==========================================================================

#[tokio::test]
#[ignore]
async fn error_get_nonexistent_doc() {
    let url = fresh_remote_db("err_noexist").await;
    let db = Database::http(&url);

    let result = db.get("does_not_exist").await;
    assert!(matches!(result, Err(RouchError::NotFound(_))));

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn error_update_wrong_rev() {
    let url = fresh_remote_db("err_wrongrev").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    // Update with a completely wrong rev
    let result = db.update("doc1", "1-bogusrevisionhash", serde_json::json!({"v": 2})).await;
    // CouchDB returns 409 Conflict for wrong rev
    assert!(result.is_err() || !result.unwrap().ok);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn error_delete_wrong_rev() {
    let url = fresh_remote_db("err_delrev").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    let result = db.remove("doc1", "1-bogusrevisionhash").await;
    assert!(result.is_err() || !result.unwrap().ok);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn error_put_existing_without_rev() {
    let url = fresh_remote_db("err_dup").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    // Second put without rev should fail (conflict)
    let result = db.put("doc1", serde_json::json!({"v": 2})).await;
    assert!(result.is_err() || !result.unwrap().ok);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn error_get_deleted_doc() {
    let url = fresh_remote_db("err_deleted").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.remove("doc1", &r1.rev.unwrap()).await.unwrap();

    let result = db.get("doc1").await;
    assert!(matches!(result, Err(RouchError::NotFound(_))));

    delete_remote_db(&url).await;
}

// ==========================================================================
// 6. all_docs advanced options
// ==========================================================================

#[tokio::test]
#[ignore]
async fn all_docs_include_docs() {
    let url = fresh_remote_db("ad_incdocs").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"name": "Alice"})).await.unwrap();
    db.put("doc2", serde_json::json!({"name": "Bob"})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    // When include_docs is true, each row should have a doc
    assert!(result.rows[0].doc.is_some());
    let doc_json = result.rows[0].doc.as_ref().unwrap();
    assert!(doc_json.get("name").is_some());

    delete_remote_db(&url).await;
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

    // Should include banana, cherry, date (inclusive_end defaults to true)
    let ids: Vec<&str> = result.rows.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"banana"));
    assert!(ids.contains(&"cherry"));
    assert!(ids.contains(&"date"));
    assert!(!ids.contains(&"apple"));
    assert!(!ids.contains(&"elderberry"));

    delete_remote_db(&url).await;
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

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn all_docs_skip_and_limit() {
    let url = fresh_remote_db("ad_paging").await;
    let db = Database::http(&url);

    for c in ["a", "b", "c", "d", "e"] {
        db.put(c, serde_json::json!({})).await.unwrap();
    }

    // Skip 1, limit 2 -> should get b, c
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

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn all_docs_empty_database() {
    let url = fresh_remote_db("ad_empty").await;
    let db = Database::http(&url);

    let result = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(result.total_rows, 0);
    assert_eq!(result.rows.len(), 0);

    delete_remote_db(&url).await;
}

// ==========================================================================
// 7. Changes feed advanced options
// ==========================================================================

#[tokio::test]
#[ignore]
async fn changes_since_sequence() {
    let url = fresh_remote_db("ch_since").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();
    db.put("doc3", serde_json::json!({"v": 3})).await.unwrap();

    // Get all changes to find the sequence after doc2
    let all = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(all.results.len(), 3);

    // Get changes since the seq of the second result
    let since_seq = all.results[1].seq.clone();
    let partial = db
        .changes(ChangesOptions {
            since: since_seq,
            ..Default::default()
        })
        .await
        .unwrap();

    // Should only get changes after that sequence
    assert!(partial.results.len() < 3);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn changes_with_limit() {
    let url = fresh_remote_db("ch_limit").await;
    let db = Database::http(&url);

    for i in 0..10 {
        db.put(&format!("doc{}", i), serde_json::json!({"i": i})).await.unwrap();
    }

    let changes = db
        .changes(ChangesOptions {
            limit: Some(3),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(changes.results.len(), 3);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn changes_include_docs() {
    let url = fresh_remote_db("ch_docs").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"name": "Alice"})).await.unwrap();

    let changes = db
        .changes(ChangesOptions {
            include_docs: true,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(changes.results.len(), 1);
    let doc = changes.results[0].doc.as_ref().unwrap();
    assert_eq!(doc["name"], "Alice");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn changes_after_updates_and_deletes() {
    let url = fresh_remote_db("ch_upddel").await;
    let db = Database::http(&url);

    // Create 3 docs
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"v": 1})).await.unwrap();
    let r3 = db.put("doc3", serde_json::json!({"v": 1})).await.unwrap();

    // Update doc1
    db.update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2})).await.unwrap();

    // Delete doc3
    db.remove("doc3", &r3.rev.unwrap()).await.unwrap();

    let changes = db.changes(ChangesOptions::default()).await.unwrap();

    // Each doc should appear once in changes (latest state)
    let ids: Vec<&str> = changes.results.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"doc1"));
    assert!(ids.contains(&"doc2"));
    assert!(ids.contains(&"doc3"));

    // doc3 should be marked as deleted
    let doc3_change = changes.results.iter().find(|r| r.id == "doc3").unwrap();
    assert!(doc3_change.deleted);

    delete_remote_db(&url).await;
}

// ==========================================================================
// 8. Conflict handling — local vs CouchDB
// ==========================================================================

#[tokio::test]
#[ignore]
async fn conflict_both_sides_modify_same_doc() {
    let url = fresh_remote_db("conflict_both").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Step 1: Create doc on local and replicate to CouchDB
    let r1 = local.put("doc1", serde_json::json!({"v": "original"})).await.unwrap();
    let original_rev = r1.rev.unwrap();
    local.replicate_to(&remote).await.unwrap();

    // Both sides now have doc1 with the same rev
    let remote_doc = remote.get("doc1").await.unwrap();
    assert_eq!(remote_doc.rev.unwrap().to_string(), original_rev);

    // Step 2: Update independently on both sides
    local
        .update("doc1", &original_rev, serde_json::json!({"v": "local_edit"}))
        .await
        .unwrap();

    remote
        .update("doc1", &original_rev, serde_json::json!({"v": "remote_edit"}))
        .await
        .unwrap();

    // Step 3: Sync — this should create a conflict
    let (push, pull) = local.sync(&remote).await.unwrap();
    assert!(push.ok);
    assert!(pull.ok);

    // Step 4: Both sides should agree on the winning revision
    let local_doc = local
        .get_with_opts("doc1", GetOptions { conflicts: true, ..Default::default() })
        .await
        .unwrap();
    let remote_doc = remote
        .get_with_opts("doc1", GetOptions { conflicts: true, ..Default::default() })
        .await
        .unwrap();

    // Winning rev must be the same (deterministic algorithm)
    assert_eq!(
        local_doc.rev.as_ref().unwrap().to_string(),
        remote_doc.rev.as_ref().unwrap().to_string(),
        "Winning revision must be the same on both sides"
    );

    // Both sides should report a conflict
    let local_conflicts = local_doc.data.get("_conflicts");
    let remote_conflicts = remote_doc.data.get("_conflicts");
    assert!(
        local_conflicts.is_some(),
        "Local should have _conflicts"
    );
    assert!(
        remote_conflicts.is_some(),
        "Remote should have _conflicts"
    );

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn conflict_local_delete_remote_update() {
    let url = fresh_remote_db("conflict_delupd").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Create and sync
    let r1 = local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev = r1.rev.unwrap();
    local.replicate_to(&remote).await.unwrap();

    // Local deletes, remote updates
    local.remove("doc1", &rev).await.unwrap();
    remote
        .update("doc1", &rev, serde_json::json!({"v": 2}))
        .await
        .unwrap();

    // Sync
    local.sync(&remote).await.unwrap();

    // The non-deleted version should win (CouchDB deterministic algorithm)
    let remote_doc = remote.get("doc1").await.unwrap();
    assert_eq!(remote_doc.data["v"], 2, "Non-deleted revision should win");

    // Local should also see the non-deleted winner
    let local_doc = local.get("doc1").await.unwrap();
    assert_eq!(local_doc.data["v"], 2, "Local should agree with remote winner");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn conflict_remote_delete_local_update() {
    let url = fresh_remote_db("conflict_updel").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Create and sync
    let r1 = local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev = r1.rev.unwrap();
    local.replicate_to(&remote).await.unwrap();

    // Remote deletes, local updates
    remote.remove("doc1", &rev).await.unwrap();
    local
        .update("doc1", &rev, serde_json::json!({"v": 2}))
        .await
        .unwrap();

    // Sync
    local.sync(&remote).await.unwrap();

    // Non-deleted should win on both sides
    let local_doc = local.get("doc1").await.unwrap();
    assert_eq!(local_doc.data["v"], 2);

    let remote_doc = remote.get("doc1").await.unwrap();
    assert_eq!(remote_doc.data["v"], 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn conflict_three_way() {
    let url = fresh_remote_db("conflict_3way").await;
    let local1 = Database::memory("local1");
    let local2 = Database::memory("local2");
    let remote = Database::http(&url);

    // Create on local1 and sync everywhere
    let r1 = local1.put("doc1", serde_json::json!({"v": "original"})).await.unwrap();
    let rev = r1.rev.unwrap();
    local1.replicate_to(&remote).await.unwrap();
    local2.replicate_from(&remote).await.unwrap();

    // All three have doc1 with same rev. Now update independently:
    local1
        .update("doc1", &rev, serde_json::json!({"v": "local1_edit"}))
        .await
        .unwrap();
    local2
        .update("doc1", &rev, serde_json::json!({"v": "local2_edit"}))
        .await
        .unwrap();
    remote
        .update("doc1", &rev, serde_json::json!({"v": "remote_edit"}))
        .await
        .unwrap();

    // Sync local1 -> remote -> local2
    local1.sync(&remote).await.unwrap();
    local2.sync(&remote).await.unwrap();
    // Sync local1 again to pick up local2's changes
    local1.sync(&remote).await.unwrap();

    // All three should agree on the winner
    let d1 = local1.get("doc1").await.unwrap();
    let d2 = local2.get("doc1").await.unwrap();
    let dr = remote.get("doc1").await.unwrap();

    let rev1 = d1.rev.as_ref().unwrap().to_string();
    let rev2 = d2.rev.as_ref().unwrap().to_string();
    let revr = dr.rev.as_ref().unwrap().to_string();

    assert_eq!(rev1, rev2, "local1 and local2 must agree on winner");
    assert_eq!(rev2, revr, "local2 and remote must agree on winner");

    // Winner data should match
    assert_eq!(d1.data["v"], d2.data["v"]);
    assert_eq!(d2.data["v"], dr.data["v"]);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn conflict_resolve_by_update() {
    let url = fresh_remote_db("conflict_resolve").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Create conflict
    let r1 = local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev = r1.rev.unwrap();
    local.replicate_to(&remote).await.unwrap();

    local.update("doc1", &rev, serde_json::json!({"v": "local"})).await.unwrap();
    remote.update("doc1", &rev, serde_json::json!({"v": "remote"})).await.unwrap();

    local.sync(&remote).await.unwrap();

    // Verify conflict exists
    let doc = local
        .get_with_opts("doc1", GetOptions { conflicts: true, ..Default::default() })
        .await
        .unwrap();
    let conflicts = doc.data.get("_conflicts")
        .and_then(|c| c.as_array())
        .expect("Should have conflicts");
    assert!(!conflicts.is_empty());

    // Resolve: update the winning rev with the resolution
    let winner_rev = doc.rev.unwrap().to_string();
    local
        .update("doc1", &winner_rev, serde_json::json!({"v": "resolved"}))
        .await
        .unwrap();

    // Replicate the resolution
    local.sync(&remote).await.unwrap();

    // Verify resolution on both sides
    let local_doc = local.get("doc1").await.unwrap();
    let remote_doc = remote.get("doc1").await.unwrap();
    assert_eq!(local_doc.data["v"], "resolved");
    assert_eq!(remote_doc.data["v"], "resolved");

    delete_remote_db(&url).await;
}

// ==========================================================================
// 9. Replication edge cases
// ==========================================================================

#[tokio::test]
#[ignore]
async fn replicate_multiple_updates_same_doc() {
    let url = fresh_remote_db("repl_multiupd").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Create and update multiple times
    let r1 = local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let r2 = local
        .update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();
    let r3 = local
        .update("doc1", &r2.rev.unwrap(), serde_json::json!({"v": 3}))
        .await
        .unwrap();
    let _r4 = local
        .update("doc1", &r3.rev.unwrap(), serde_json::json!({"v": 4}))
        .await
        .unwrap();

    // Replicate all at once
    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 4);
    // Rev should be at generation 4
    assert!(doc.rev.unwrap().to_string().starts_with("4-"));

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_delete_and_recreate() {
    let url = fresh_remote_db("repl_delrec").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Create, sync, delete
    let r1 = local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    local.replicate_to(&remote).await.unwrap();
    local.remove("doc1", &r1.rev.unwrap()).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    // Verify deleted
    assert!(remote.get("doc1").await.is_err());

    // "Recreate" by putting a new doc with the same ID
    // We need to use bulk_docs with new_edits=false and a new revision tree,
    // or just put a brand new doc (which will fail since the old one exists as tombstone).
    // In PouchDB, you'd start from the tombstone's rev.
    // Let's get the tombstone rev from the local adapter and build from there.
    let _local_doc = local.adapter().get("doc1", GetOptions {
        rev: None,
        conflicts: false,
        open_revs: None,
        revs: true,
    }).await;

    // The doc is deleted, so get returns NotFound. Let's use changes to find the rev.
    let changes = local.changes(ChangesOptions::default()).await.unwrap();
    let doc1_change = changes.results.iter().find(|r| r.id == "doc1").unwrap();
    let tombstone_rev = &doc1_change.changes[0].rev;

    // Update the tombstone to "un-delete"
    local
        .update("doc1", tombstone_rev, serde_json::json!({"v": "resurrected"}))
        .await
        .unwrap();

    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], "resurrected");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_empty_databases() {
    let url = fresh_remote_db("repl_empty").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_read, 0);
    assert_eq!(result.docs_written, 0);

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_read, 0);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_couchdb_to_redb() {
    let url = fresh_remote_db("couch_to_redb").await;
    let remote = Database::http(&url);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let local = Database::open(&path, "local").unwrap();

    remote.put("doc1", serde_json::json!({"source": "couchdb"})).await.unwrap();
    remote.put("doc2", serde_json::json!({"source": "couchdb"})).await.unwrap();

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 2);

    let doc = local.get("doc1").await.unwrap();
    assert_eq!(doc.data["source"], "couchdb");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_redb_bidirectional_memory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let redb = Database::open(&path, "redb").unwrap();
    let memory = Database::memory("mem");

    redb.put("from_redb", serde_json::json!({"source": "redb"})).await.unwrap();
    memory.put("from_mem", serde_json::json!({"source": "memory"})).await.unwrap();

    let (push, pull) = redb.sync(&memory).await.unwrap();
    assert!(push.ok);
    assert!(pull.ok);

    // Both should have both docs
    let _ = redb.get("from_mem").await.unwrap();
    let _ = memory.get("from_redb").await.unwrap();

    let redb_info = redb.info().await.unwrap();
    let mem_info = memory.info().await.unwrap();
    assert_eq!(redb_info.doc_count, 2);
    assert_eq!(mem_info.doc_count, 2);
}

#[tokio::test]
#[ignore]
async fn replicate_single_doc_batches() {
    let url = fresh_remote_db("batch_one").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    for i in 0..5 {
        local.put(&format!("doc{}", i), serde_json::json!({"i": i})).await.unwrap();
    }

    // batch_size=1 means each doc is its own batch
    let result = local
        .replicate_to_with_opts(
            &remote,
            ReplicationOptions {
                batch_size: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 5);

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 5);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_exact_batch_boundary() {
    let url = fresh_remote_db("batch_exact").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Exactly batch_size docs
    for i in 0..10 {
        local.put(&format!("doc{:02}", i), serde_json::json!({"i": i})).await.unwrap();
    }

    let result = local
        .replicate_to_with_opts(
            &remote,
            ReplicationOptions {
                batch_size: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 10);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_large_batch() {
    let url = fresh_remote_db("batch_large").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    for i in 0..200 {
        local.put(&format!("doc{:04}", i), serde_json::json!({"i": i})).await.unwrap();
    }

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 200);

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 200);

    delete_remote_db(&url).await;
}

// ==========================================================================
// 10. Mango queries — comprehensive operator coverage
// ==========================================================================

#[tokio::test]
#[ignore]
async fn mango_equality_and_inequality() {
    let url = fresh_remote_db("mango_eq").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"name": "Alice", "age": 30})).await.unwrap();
    remote.put("b", serde_json::json!({"name": "Bob", "age": 25})).await.unwrap();
    remote.put("c", serde_json::json!({"name": "Charlie", "age": 30})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    // $eq
    let result = local.find(FindOptions {
        selector: serde_json::json!({"age": {"$eq": 30}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    // Implicit $eq
    let result = local.find(FindOptions {
        selector: serde_json::json!({"name": "Bob"}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["name"], "Bob");

    // $ne
    let result = local.find(FindOptions {
        selector: serde_json::json!({"age": {"$ne": 30}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["name"], "Bob");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_comparison_operators() {
    let url = fresh_remote_db("mango_cmp").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"score": 10})).await.unwrap();
    remote.put("b", serde_json::json!({"score": 20})).await.unwrap();
    remote.put("c", serde_json::json!({"score": 30})).await.unwrap();
    remote.put("d", serde_json::json!({"score": 40})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    // $gt
    let result = local.find(FindOptions {
        selector: serde_json::json!({"score": {"$gt": 20}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    // $gte
    let result = local.find(FindOptions {
        selector: serde_json::json!({"score": {"$gte": 20}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 3);

    // $lt
    let result = local.find(FindOptions {
        selector: serde_json::json!({"score": {"$lt": 30}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    // Range combination
    let result = local.find(FindOptions {
        selector: serde_json::json!({"score": {"$gte": 20, "$lt": 40}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_in_nin_exists() {
    let url = fresh_remote_db("mango_in").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"color": "red", "size": 10})).await.unwrap();
    remote.put("b", serde_json::json!({"color": "blue", "size": 20})).await.unwrap();
    remote.put("c", serde_json::json!({"color": "green"})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    // $in
    let result = local.find(FindOptions {
        selector: serde_json::json!({"color": {"$in": ["red", "blue"]}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    // $nin
    let result = local.find(FindOptions {
        selector: serde_json::json!({"color": {"$nin": ["red"]}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    // $exists
    let result = local.find(FindOptions {
        selector: serde_json::json!({"size": {"$exists": true}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local.find(FindOptions {
        selector: serde_json::json!({"size": {"$exists": false}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["color"], "green");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_logical_operators() {
    let url = fresh_remote_db("mango_logic").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"x": 1, "y": "a"})).await.unwrap();
    remote.put("b", serde_json::json!({"x": 2, "y": "b"})).await.unwrap();
    remote.put("c", serde_json::json!({"x": 3, "y": "a"})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    // $or
    let result = local.find(FindOptions {
        selector: serde_json::json!({"$or": [{"x": 1}, {"x": 3}]}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    // $and
    let result = local.find(FindOptions {
        selector: serde_json::json!({"$and": [{"y": "a"}, {"x": {"$gt": 1}}]}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["x"], 3);

    // $not
    let result = local.find(FindOptions {
        selector: serde_json::json!({"x": {"$not": {"$eq": 2}}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    // $nor
    let result = local.find(FindOptions {
        selector: serde_json::json!({"$nor": [{"x": 1}, {"x": 2}]}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["x"], 3);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_nested_field_query() {
    let url = fresh_remote_db("mango_nested").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({
        "address": {"city": "NYC", "state": "NY"}
    })).await.unwrap();
    remote.put("b", serde_json::json!({
        "address": {"city": "LA", "state": "CA"}
    })).await.unwrap();
    remote.put("c", serde_json::json!({
        "address": {"city": "SF", "state": "CA"}
    })).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local.find(FindOptions {
        selector: serde_json::json!({"address.state": "CA"}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local.find(FindOptions {
        selector: serde_json::json!({"address.city": "NYC"}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_regex_and_type() {
    let url = fresh_remote_db("mango_regex").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"email": "alice@example.com"})).await.unwrap();
    remote.put("b", serde_json::json!({"email": "bob@test.org"})).await.unwrap();
    remote.put("c", serde_json::json!({"email": 12345})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    // $regex
    let result = local.find(FindOptions {
        selector: serde_json::json!({"email": {"$regex": ".*@example\\.com$"}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["email"], "alice@example.com");

    // $type
    let result = local.find(FindOptions {
        selector: serde_json::json!({"email": {"$type": "string"}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local.find(FindOptions {
        selector: serde_json::json!({"email": {"$type": "number"}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_array_operators() {
    let url = fresh_remote_db("mango_arr").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"tags": ["rust", "db"]})).await.unwrap();
    remote.put("b", serde_json::json!({"tags": ["python", "web", "db"]})).await.unwrap();
    remote.put("c", serde_json::json!({"tags": ["rust", "web", "db"]})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    // $all - must contain all specified elements
    let result = local.find(FindOptions {
        selector: serde_json::json!({"tags": {"$all": ["rust", "db"]}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    // $size
    let result = local.find(FindOptions {
        selector: serde_json::json!({"tags": {"$size": 3}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);

    let result = local.find(FindOptions {
        selector: serde_json::json!({"tags": {"$size": 2}}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_sort_skip_limit_projection() {
    let url = fresh_remote_db("mango_sort").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"name": "Alice", "age": 30, "city": "NYC"})).await.unwrap();
    remote.put("b", serde_json::json!({"name": "Bob", "age": 25, "city": "LA"})).await.unwrap();
    remote.put("c", serde_json::json!({"name": "Charlie", "age": 35, "city": "SF"})).await.unwrap();
    remote.put("d", serde_json::json!({"name": "Diana", "age": 28, "city": "NYC"})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    // Sort ascending by age
    let result = local.find(FindOptions {
        selector: serde_json::json!({}),
        sort: Some(vec![SortField::Simple("age".into())]),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs[0]["name"], "Bob");    // 25
    assert_eq!(result.docs[3]["name"], "Charlie"); // 35

    // Sort descending by age
    let mut dir = HashMap::new();
    dir.insert("age".to_string(), "desc".to_string());
    let result = local.find(FindOptions {
        selector: serde_json::json!({}),
        sort: Some(vec![SortField::WithDirection(dir)]),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs[0]["name"], "Charlie"); // 35

    // Skip and limit
    let result = local.find(FindOptions {
        selector: serde_json::json!({}),
        sort: Some(vec![SortField::Simple("age".into())]),
        skip: Some(1),
        limit: Some(2),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 2);
    assert_eq!(result.docs[0]["name"], "Diana");  // 28 (skipped Bob 25)

    // Projection
    let result = local.find(FindOptions {
        selector: serde_json::json!({"name": "Alice"}),
        fields: Some(vec!["name".into(), "age".into()]),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["name"], "Alice");
    assert_eq!(result.docs[0]["age"], 30);
    // "city" should not be present in projection
    assert!(result.docs[0].get("city").is_none());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_empty_selector_matches_all() {
    let url = fresh_remote_db("mango_empty").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"v": 1})).await.unwrap();
    remote.put("b", serde_json::json!({"v": 2})).await.unwrap();
    remote.put("c", serde_json::json!({"v": 3})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local.find(FindOptions {
        selector: serde_json::json!({}),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(result.docs.len(), 3);

    delete_remote_db(&url).await;
}

// ==========================================================================
// 11. Map/reduce views on replicated data
// ==========================================================================

#[tokio::test]
#[ignore]
async fn view_basic_map() {
    let url = fresh_remote_db("view_map").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"type": "person", "name": "Alice", "age": 30})).await.unwrap();
    remote.put("b", serde_json::json!({"type": "person", "name": "Bob", "age": 25})).await.unwrap();
    remote.put("c", serde_json::json!({"type": "city", "name": "NYC"})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    // Map: emit name for all "person" type docs
    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        if doc.get("type").and_then(|t| t.as_str()) == Some("person") {
            vec![(
                doc["name"].clone(),
                doc["age"].clone(),
            )]
        } else {
            vec![]
        }
    };

    let results = query_view(
        local.adapter(),
        &map_fn,
        None,
        ViewQueryOptions::new(),
    )
    .await
    .unwrap();

    assert_eq!(results.rows.len(), 2);
    // Results sorted by key (name): Alice, Bob
    assert_eq!(results.rows[0].key, "Alice");
    assert_eq!(results.rows[0].value, 30);
    assert_eq!(results.rows[1].key, "Bob");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn view_reduce_sum_and_count() {
    let url = fresh_remote_db("view_reduce").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote.put("a", serde_json::json!({"dept": "eng", "salary": 100})).await.unwrap();
    remote.put("b", serde_json::json!({"dept": "eng", "salary": 120})).await.unwrap();
    remote.put("c", serde_json::json!({"dept": "sales", "salary": 90})).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        vec![(
            doc["dept"].clone(),
            doc["salary"].clone(),
        )]
    };

    // Sum all salaries
    let results = query_view(
        local.adapter(),
        &map_fn,
        Some(&ReduceFn::Sum),
        ViewQueryOptions { reduce: true, ..ViewQueryOptions::new() },
    )
    .await
    .unwrap();

    assert_eq!(results.rows.len(), 1);
    assert_eq!(results.rows[0].value, 310.0);

    // Count
    let results = query_view(
        local.adapter(),
        &map_fn,
        Some(&ReduceFn::Count),
        ViewQueryOptions { reduce: true, ..ViewQueryOptions::new() },
    )
    .await
    .unwrap();
    assert_eq!(results.rows[0].value, 3);

    // Group by department
    let results = query_view(
        local.adapter(),
        &map_fn,
        Some(&ReduceFn::Sum),
        ViewQueryOptions { reduce: true, group: true, ..ViewQueryOptions::new() },
    )
    .await
    .unwrap();

    assert_eq!(results.rows.len(), 2);
    let eng = results.rows.iter().find(|r| r.key == "eng").unwrap();
    assert_eq!(eng.value, 220.0);
    let sales = results.rows.iter().find(|r| r.key == "sales").unwrap();
    assert_eq!(sales.value, 90.0);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn view_key_range() {
    let url = fresh_remote_db("view_range").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    for i in 0..10 {
        remote.put(&format!("d{}", i), serde_json::json!({"n": i})).await.unwrap();
    }

    local.replicate_from(&remote).await.unwrap();

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        vec![(doc["n"].clone(), serde_json::json!(1))]
    };

    // Key range [3, 7]
    let results = query_view(
        local.adapter(),
        &map_fn,
        None,
        ViewQueryOptions {
            start_key: Some(serde_json::json!(3)),
            end_key: Some(serde_json::json!(7)),
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    assert_eq!(results.rows.len(), 5); // 3, 4, 5, 6, 7

    delete_remote_db(&url).await;
}

// ==========================================================================
// 12. Attachments via HTTP
// ==========================================================================

#[tokio::test]
#[ignore]
async fn attachment_put_and_get_http() {
    let url = fresh_remote_db("attach").await;
    let db = Database::http(&url);

    // Create doc first
    let r1 = db.put("doc1", serde_json::json!({"name": "test"})).await.unwrap();
    let rev = r1.rev.unwrap();

    // Put binary attachment
    let data = b"Hello, CouchDB attachments!".to_vec();
    let result = db
        .adapter()
        .put_attachment("doc1", "greeting.txt", &rev, data.clone(), "text/plain")
        .await
        .unwrap();
    assert!(result.ok);

    // Get attachment back
    let retrieved = db
        .adapter()
        .get_attachment("doc1", "greeting.txt", GetAttachmentOptions::default())
        .await
        .unwrap();

    assert_eq!(retrieved, data);

    // Verify doc still has its data
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["name"], "test");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn attachment_binary_data() {
    let url = fresh_remote_db("attach_bin").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({})).await.unwrap();
    let rev = r1.rev.unwrap();

    // Put binary data (non-UTF8)
    let binary_data: Vec<u8> = (0..=255).collect();
    let result = db
        .adapter()
        .put_attachment("doc1", "bytes.bin", &rev, binary_data.clone(), "application/octet-stream")
        .await
        .unwrap();
    assert!(result.ok);

    let retrieved = db
        .adapter()
        .get_attachment("doc1", "bytes.bin", GetAttachmentOptions::default())
        .await
        .unwrap();

    assert_eq!(retrieved, binary_data);

    delete_remote_db(&url).await;
}

// ==========================================================================
// 13. Database operations
// ==========================================================================

#[tokio::test]
#[ignore]
async fn database_info_http() {
    let url = fresh_remote_db("db_info").await;
    let db = Database::http(&url);

    // Empty DB
    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 0);

    db.put("doc1", serde_json::json!({})).await.unwrap();
    db.put("doc2", serde_json::json!({})).await.unwrap();

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn database_compact_http() {
    let url = fresh_remote_db("db_compact").await;
    let db = Database::http(&url);

    // Create and update to generate revisions
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let r2 = db.update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2})).await.unwrap();
    db.update("doc1", &r2.rev.unwrap(), serde_json::json!({"v": 3})).await.unwrap();

    // Compact should not lose data
    db.compact().await.unwrap();

    // Short delay for compaction to complete
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 3);

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn database_destroy_http() {
    let url = fresh_remote_db("db_destroy").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    // Destroy should delete the database
    db.destroy().await.unwrap();

    // Getting info should fail (database doesn't exist)
    let result = db.info().await;
    assert!(result.is_err());

    // No need to delete_remote_db since we just destroyed it
}

// ==========================================================================
// 14. Cross-adapter data fidelity
// ==========================================================================

#[tokio::test]
#[ignore]
async fn cross_adapter_fidelity_memory_couchdb_redb() {
    let url = fresh_remote_db("fidelity").await;
    let memory = Database::memory("mem");
    let remote = Database::http(&url);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let redb = Database::open(&path, "redb").unwrap();

    // Rich document with diverse data types
    let data = serde_json::json!({
        "string": "hello",
        "int": 42,
        "float": 3.14,
        "bool_t": true,
        "bool_f": false,
        "null_val": null,
        "array": [1, "two", null],
        "nested": {"a": {"b": {"c": "deep"}}},
        "empty_arr": [],
        "empty_obj": {}
    });

    // Memory -> CouchDB -> Redb -> verify all match
    memory.put("doc1", data.clone()).await.unwrap();
    memory.replicate_to(&remote).await.unwrap();
    redb.replicate_from(&remote).await.unwrap();

    let mem_doc = memory.get("doc1").await.unwrap();
    let remote_doc = remote.get("doc1").await.unwrap();
    let redb_doc = redb.get("doc1").await.unwrap();

    // All three should have identical data
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

    delete_remote_db(&url).await;
}

// ==========================================================================
// 15. Replication with updates pulled back from CouchDB
// ==========================================================================

#[tokio::test]
#[ignore]
async fn replicate_remote_updates_back_to_local() {
    let url = fresh_remote_db("pull_updates").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Push to remote
    local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    // Update on remote
    let remote_doc = remote.get("doc1").await.unwrap();
    let remote_rev = remote_doc.rev.unwrap().to_string();
    remote.update("doc1", &remote_rev, serde_json::json!({"v": 2})).await.unwrap();

    // Pull back
    local.replicate_from(&remote).await.unwrap();

    let local_doc = local.get("doc1").await.unwrap();
    assert_eq!(local_doc.data["v"], 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_remote_deletes_back_to_local() {
    let url = fresh_remote_db("pull_deletes").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Push to remote
    local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    // Delete on remote
    let remote_doc = remote.get("doc1").await.unwrap();
    let remote_rev = remote_doc.rev.unwrap().to_string();
    remote.remove("doc1", &remote_rev).await.unwrap();

    // Pull back
    local.replicate_from(&remote).await.unwrap();

    // Local should see deletion
    let result = local.get("doc1").await;
    assert!(result.is_err());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn sync_interleaved_updates() {
    let url = fresh_remote_db("interleave").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Create doc on local, sync
    let r1 = local.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    local.sync(&remote).await.unwrap();

    // Update on local, sync
    let _r2 = local.update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2})).await.unwrap();
    local.sync(&remote).await.unwrap();

    // Update on remote, sync
    let remote_doc = remote.get("doc1").await.unwrap();
    let remote_rev = remote_doc.rev.unwrap().to_string();
    remote.update("doc1", &remote_rev, serde_json::json!({"v": 3})).await.unwrap();
    local.sync(&remote).await.unwrap();

    // Update on local again, sync
    let local_doc = local.get("doc1").await.unwrap();
    let local_rev = local_doc.rev.unwrap().to_string();
    local.update("doc1", &local_rev, serde_json::json!({"v": 4})).await.unwrap();
    local.sync(&remote).await.unwrap();

    // Both should agree on v=4
    let final_local = local.get("doc1").await.unwrap();
    let final_remote = remote.get("doc1").await.unwrap();
    assert_eq!(final_local.data["v"], 4);
    assert_eq!(final_remote.data["v"], 4);
    assert_eq!(
        final_local.rev.unwrap().to_string(),
        final_remote.rev.unwrap().to_string()
    );

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn sync_many_docs_diverse_operations() {
    let url = fresh_remote_db("diverse_ops").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Create 10 docs locally
    let mut revs = Vec::new();
    for i in 0..10 {
        let r = local.put(&format!("doc{:02}", i), serde_json::json!({"i": i})).await.unwrap();
        revs.push(r.rev.unwrap());
    }
    local.sync(&remote).await.unwrap();

    // Update even-numbered docs
    for i in (0..10).step_by(2) {
        local
            .update(&format!("doc{:02}", i), &revs[i], serde_json::json!({"i": i, "updated": true}))
            .await
            .unwrap();
    }

    // Delete odd-numbered docs
    for i in (1..10).step_by(2) {
        local.remove(&format!("doc{:02}", i), &revs[i]).await.unwrap();
    }

    // Sync all changes
    local.sync(&remote).await.unwrap();

    // Verify remote state
    let remote_info = remote.info().await.unwrap();
    assert_eq!(remote_info.doc_count, 5); // 5 remaining after deletes

    for i in (0..10).step_by(2) {
        let doc = remote.get(&format!("doc{:02}", i)).await.unwrap();
        assert_eq!(doc.data["updated"], true);
    }

    for i in (1..10).step_by(2) {
        let result = remote.get(&format!("doc{:02}", i)).await;
        assert!(result.is_err(), "doc{:02} should be deleted", i);
    }

    delete_remote_db(&url).await;
}
