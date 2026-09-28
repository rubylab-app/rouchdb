//! Document data diversity: type roundtrips through CouchDB and special IDs.

mod common;

use common::fresh_remote_db;
use rouchdb::Database;

// =========================================================================
// Data type roundtrips
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_nested_objects_roundtrip() {
    let url = fresh_remote_db("data_nested").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let data = serde_json::json!({
        "address": {
            "street": "123 Main St",
            "city": "New York",
            "geo": { "lat": 40.7128, "lng": -74.0060 }
        },
        "contacts": {
            "email": "alice@example.com",
            "phones": { "home": "555-0100", "work": "555-0200" }
        }
    });

    local.put("doc1", data.clone()).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["address"]["city"], "New York");
    assert_eq!(doc.data["address"]["geo"]["lat"], 40.7128);
    assert_eq!(doc.data["contacts"]["phones"]["work"], "555-0200");

    let local2 = Database::memory("local2");
    local2.replicate_from(&remote).await.unwrap();
    let doc2 = local2.get("doc1").await.unwrap();
    assert_eq!(doc2.data["address"]["geo"]["lng"], -74.006);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_numeric_types_roundtrip() {
    let url = fresh_remote_db("data_nums").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let data = serde_json::json!({
        "integer": 42,
        "negative": -7,
        "zero": 0,
        "float": 3.14160,
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
    assert!((doc.data["float"].as_f64().unwrap() - 3.14160).abs() < 1e-10);
    assert_eq!(doc.data["small_float"], 0.001);
    assert_eq!(doc.data["negative_float"], -273.15);
    assert_eq!(doc.data["big"], 9999999999_i64);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert_eq!(
        doc.data["accented"],
        "caf\u{00E9} na\u{00EF}ve r\u{00E9}sum\u{00E9}"
    );
    assert_eq!(doc.data["special_chars"], "line1\nline2\ttab\\backslash");
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_large_document() {
    let url = fresh_remote_db("data_large").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let mut obj = serde_json::Map::new();
    for i in 0..100 {
        obj.insert(
            format!("field_{}", i),
            serde_json::json!({
                "index": i,
                "value": format!("value_{}", i),
                "nested": {"depth": 1, "data": [i, i*2, i*3]}
            }),
        );
    }
    let data = serde_json::Value::Object(obj);

    local.put("big_doc", data).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("big_doc").await.unwrap();
    assert_eq!(doc.data["field_0"]["index"], 0);
    assert_eq!(doc.data["field_99"]["value"], "value_99");
    assert_eq!(doc.data["field_50"]["nested"]["data"][2], 150);
}

// =========================================================================
// Special document IDs
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn special_id_with_spaces() {
    let url = fresh_remote_db("id_spaces").await;
    let db = Database::http(&url);

    db.put("my document", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    let doc = db.get("my document").await.unwrap();
    assert_eq!(doc.data["v"], 1);
    assert_eq!(doc.id, "my document");
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn special_id_with_unicode() {
    let url = fresh_remote_db("id_unicode").await;
    let db = Database::http(&url);

    db.put("doc_\u{00E9}\u{00E8}\u{00EA}", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    let doc = db.get("doc_\u{00E9}\u{00E8}\u{00EA}").await.unwrap();
    assert_eq!(doc.data["v"], 1);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn special_id_replicate_roundtrip() {
    let url = fresh_remote_db("id_repl").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local
        .put("has spaces", serde_json::json!({"t": "spaces"}))
        .await
        .unwrap();
    local
        .put("has/slash", serde_json::json!({"t": "slash"}))
        .await
        .unwrap();
    local
        .put("has+plus", serde_json::json!({"t": "plus"}))
        .await
        .unwrap();
    local
        .put("has?question", serde_json::json!({"t": "question"}))
        .await
        .unwrap();

    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("has spaces").await.unwrap();
    assert_eq!(doc.data["t"], "spaces");

    let doc = remote.get("has+plus").await.unwrap();
    assert_eq!(doc.data["t"], "plus");
}
