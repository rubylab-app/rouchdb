//! Document data diversity: type roundtrips through CouchDB and special IDs.
//!
//! Each document is written on the memory backend, pushed to CouchDB, and
//! pulled from CouchDB into a fresh memory and a redb database; every copy
//! must have exactly the written body under the same revision.

mod common;

use common::fresh_remote_db;
use rouchdb::{AllDocsOptions, Database};

/// Replicates `docs` memory → CouchDB → (memory, redb) and checks every
/// copy of every document.
async fn assert_roundtrip(prefix: &str, docs: &[(&str, serde_json::Value)]) {
    let url = fresh_remote_db(prefix).await;
    let local = Database::memory("local");
    let remote = Database::http(&url);
    let pulled = Database::memory("pulled");
    let dir = tempfile::tempdir().unwrap();
    let redb = Database::open(dir.path().join("pulled.redb"), "pulled").unwrap();

    let mut written = Vec::new();
    for (id, body) in docs {
        let r = local.put(id, body.clone()).await.unwrap();
        written.push((id.to_string(), r.rev.unwrap(), body.clone()));
    }
    let push = local.replicate_to(&remote).await.unwrap();
    assert_eq!(push.docs_written, docs.len() as u64);
    pulled.replicate_from(&remote).await.unwrap();
    redb.replicate_from(&remote).await.unwrap();

    let mut ids: Vec<String> = docs.iter().map(|(id, _)| id.to_string()).collect();
    ids.sort();
    for (db, name) in [
        (&local, "memory"),
        (&remote, "couchdb"),
        (&pulled, "pulled memory"),
        (&redb, "pulled redb"),
    ] {
        let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
        let mut listed: Vec<String> = all.rows.iter().map(|r| r.id.clone()).collect();
        listed.sort();
        assert_eq!(listed, ids, "{name}");
        for (id, rev, body) in &written {
            let doc = db.get(id).await.unwrap();
            assert_eq!(doc.id, *id, "{name}");
            assert_eq!(doc.rev.unwrap().to_string(), *rev, "{name} {id}");
            assert_eq!(doc.data, *body, "{name} {id}");
        }
    }
}

// =========================================================================
// Data type roundtrips
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_nested_objects_roundtrip() {
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
    assert_roundtrip("data_nested", &[("doc1", data)]).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_arrays_roundtrip() {
    let data = serde_json::json!({
        "tags": ["rust", "database", "sync"],
        "matrix": [[1, 2, 3], [4, 5, 6]],
        "nested": [{"name": "a"}, {"name": "b"}]
    });
    assert_roundtrip("data_arrays", &[("doc1", data)]).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_null_and_bool_roundtrip() {
    let data = serde_json::json!({
        "optional": null,
        "nested_null": {"inner": null},
        "active": true,
        "deleted": false,
        "flags": [true, false, null]
    });
    assert_roundtrip("data_nullbool", &[("doc1", data)]).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_numeric_types_roundtrip() {
    let data = serde_json::json!({
        "integer": 42,
        "negative": -7,
        "zero": 0,
        "float": 3.14160,
        "small_float": 0.001,
        "negative_float": -273.15,
        "exponent": 1.5e-7,
        "big": 9999999999_i64,
        "max_safe": 9007199254740991_i64
    });
    assert_roundtrip("data_nums", &[("doc1", data)]).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_empty_structures_roundtrip() {
    let data = serde_json::json!({
        "empty_arr": [],
        "empty_obj": {},
        "empty_str": "",
        "nested_empty": {"a": [], "b": {}}
    });
    assert_roundtrip("data_empty", &[("doc1", data)]).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_mixed_type_array_roundtrip() {
    let data = serde_json::json!({
        "mix": [1, "two", true, null, {"nested": 5}, [6, 7]]
    });
    assert_roundtrip("data_mixed", &[("doc1", data)]).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_unicode_roundtrip() {
    let data = serde_json::json!({
        "emoji": "\u{1F980}\u{1F389}",
        "japanese": "\u{6771}\u{4EAC}",
        "chinese": "\u{4F60}\u{597D}\u{4E16}\u{754C}",
        "korean": "\u{C548}\u{B155}\u{D558}\u{C138}\u{C694}",
        "arabic": "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}",
        "accented": "caf\u{00E9} na\u{00EF}ve r\u{00E9}sum\u{00E9}",
        "special_chars": "line1\nline2\ttab\\backslash\"quote\u{0001}",
        "\u{1F511}": "unicode key"
    });
    assert_roundtrip("data_unicode", &[("doc1", data)]).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn data_large_document() {
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
    obj.insert("text".into(), serde_json::json!("x".repeat(100_000)));
    assert_roundtrip("data_large", &[("big_doc", serde_json::Value::Object(obj))]).await;
}

// =========================================================================
// Special document IDs
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn special_id_with_spaces() {
    let url = fresh_remote_db("id_spaces").await;
    let db = Database::http(&url);

    let r = db
        .put("my document", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    assert_eq!(r.id, "my document");
    let doc = db.get("my document").await.unwrap();
    assert_eq!(doc.id, "my document");
    assert_eq!(doc.rev.unwrap().to_string(), r.rev.unwrap());
    assert_eq!(doc.data, serde_json::json!({"v": 1}));
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn special_id_with_unicode() {
    let url = fresh_remote_db("id_unicode").await;
    let db = Database::http(&url);

    let id = "doc_\u{00E9}\u{00E8}\u{00EA}_\u{1F600}";
    let r = db.put(id, serde_json::json!({"v": 1})).await.unwrap();
    assert_eq!(r.id, id);
    let doc = db.get(id).await.unwrap();
    assert_eq!(doc.id, id);
    assert_eq!(doc.data, serde_json::json!({"v": 1}));
    let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(all.rows.len(), 1);
    assert_eq!(all.rows[0].id, id);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn special_id_replicate_roundtrip() {
    let docs: Vec<(&str, serde_json::Value)> = [
        ("has spaces", "spaces"),
        ("has/slash", "slash"),
        ("has+plus", "plus"),
        ("has?question", "question"),
        ("has#hash", "hash"),
        ("has%25percent", "percent"),
        ("has&amp", "amp"),
        ("has\u{e9}accent", "accent"),
    ]
    .into_iter()
    .map(|(id, t)| (id, serde_json::json!({"t": t})))
    .collect();
    assert_roundtrip("id_repl", &docs).await;
}
