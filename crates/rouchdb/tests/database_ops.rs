//! Database operations (info, compact, destroy) and cross-adapter fidelity.

mod common;

use common::fresh_remote_db;
use rouchdb::{AllDocsOptions, Database, GetOptions, RouchError};

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn database_info_http() {
    let url = fresh_remote_db("db_info").await;
    let db = Database::http(&url);

    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (0, 0));

    db.put("doc1", serde_json::json!({})).await.unwrap();
    let r2 = db.put("doc2", serde_json::json!({})).await.unwrap();
    db.put("doc3", serde_json::json!({})).await.unwrap();
    db.remove("doc2", &r2.rev.unwrap()).await.unwrap();

    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (2, 1));
    assert_eq!(info.update_seq.as_num(), 4);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn database_compact_http() {
    let url = fresh_remote_db("db_compact").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let r2 = db
        .update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();
    let r3 = db
        .update("doc1", &r2.rev.unwrap(), serde_json::json!({"v": 3}))
        .await
        .unwrap();

    // CouchDB compacts in the background; the reads below do not depend
    // on when it finishes, so there is nothing to wait for.
    db.compact().await.unwrap();

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.rev.unwrap().to_string(), r3.rev.unwrap());
    assert_eq!(doc.data, serde_json::json!({"v": 3}));

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 1);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn database_destroy_http() {
    let url = fresh_remote_db("db_destroy").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    db.destroy().await.unwrap();

    // The database is gone from the server...
    let status = reqwest::Client::new()
        .get(url.url())
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 404);
    // ...and, like the local adapters, the handle then behaves as a new,
    // empty database: it is re-created on its next use.
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (0, 0));
    assert!(matches!(db.get("doc1").await, Err(RouchError::NotFound(_))));
    let r = db.put("doc1", serde_json::json!({"v": 2})).await.unwrap();
    assert!(r.rev.unwrap().starts_with("1-"));
    db.destroy().await.unwrap();
}

/// Documents written on the memory backend, replicated to CouchDB and from
/// there to redb, are the same documents (body and revision) everywhere.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn cross_adapter_fidelity_memory_couchdb_redb() {
    let url = fresh_remote_db("fidelity").await;
    let memory = Database::memory("mem");
    let remote = Database::http(&url);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let redb = Database::open(&path, "redb").unwrap();

    let docs = [
        (
            "types",
            serde_json::json!({
                "string": "hello",
                "int": 42,
                "negative": -7,
                "float": 3.15,
                "small_float": 0.001,
                "bool_t": true,
                "bool_f": false,
                "null_val": null,
                "array": [1, "two", null, [3.5, {"k": false}]],
                "nested": {"a": {"b": {"c": "deep"}}},
                "empty_arr": [],
                "empty_obj": {},
                "empty_str": "",
                "unicode": "caf\u{e9} \u{1F980} \u{6771}\u{4EAC}",
                "escapes": "line1\nline2\t\"quoted\"\\"
            }),
        ),
        ("with space/and+plus", serde_json::json!({"v": 1})),
        ("\u{e9}t\u{e9}", serde_json::json!({"v": 2})),
        ("updated", serde_json::json!({"v": 1})),
    ];
    let mut revs = std::collections::BTreeMap::new();
    let mut bodies = std::collections::BTreeMap::new();
    for (id, body) in &docs {
        let r = memory.put(id, body.clone()).await.unwrap();
        revs.insert(id.to_string(), r.rev.unwrap());
        bodies.insert(id.to_string(), body.clone());
    }
    // A document with history and one that was deleted.
    let updated = serde_json::json!({"v": 2, "history": true});
    let r = memory
        .update("updated", &revs["updated"], updated.clone())
        .await
        .unwrap();
    revs.insert("updated".into(), r.rev.unwrap());
    bodies.insert("updated".into(), updated);
    let gone = memory
        .put("gone", serde_json::json!({"v": 0}))
        .await
        .unwrap();
    let tombstone = memory
        .remove("gone", &gone.rev.unwrap())
        .await
        .unwrap()
        .rev
        .unwrap();

    // A document with an attachment.
    let with_att = memory
        .put("attached", serde_json::json!({"v": 3}))
        .await
        .unwrap();
    let bytes: Vec<u8> = (0..=255).collect();
    let r = memory
        .put_attachment(
            "attached",
            "bytes.bin",
            &with_att.rev.unwrap(),
            bytes.clone(),
            "application/octet-stream",
        )
        .await
        .unwrap();
    revs.insert("attached".into(), r.rev.unwrap());
    bodies.insert("attached".into(), serde_json::json!({"v": 3}));

    memory.replicate_to(&remote).await.unwrap();
    redb.replicate_from(&remote).await.unwrap();

    let ids: Vec<String> = bodies.keys().cloned().collect();
    let digest = memory.get("attached").await.unwrap().attachments["bytes.bin"]
        .digest
        .clone();
    assert!(digest.starts_with("md5-"), "{digest}");
    for (db, name) in [(&memory, "memory"), (&remote, "couchdb"), (&redb, "redb")] {
        let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
        let mut listed: Vec<String> = all.rows.iter().map(|r| r.key.clone()).collect();
        listed.sort();
        assert_eq!(listed, ids, "{name}");
        for (id, body) in &bodies {
            let doc = db.get(id).await.unwrap();
            assert_eq!(doc.rev.unwrap().to_string(), revs[id], "{name} {id}");
            assert_eq!(&doc.data, body, "{name} {id}");
        }
        let attached = db.get("attached").await.unwrap();
        let meta = &attached.attachments["bytes.bin"];
        assert_eq!(
            (
                meta.content_type.as_str(),
                meta.length,
                meta.stub,
                &meta.digest
            ),
            ("application/octet-stream", 256, true, &digest),
            "{name}"
        );
        assert_eq!(
            db.get_attachment("attached", "bytes.bin").await.unwrap(),
            bytes,
            "{name}"
        );
        assert!(
            matches!(db.get("gone").await, Err(RouchError::NotFound(_))),
            "{name}"
        );
        let tomb = db
            .get_with_opts(
                "gone",
                GetOptions {
                    rev: Some(tombstone.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(tomb.deleted, "{name}");
        let info = db.info().await.unwrap();
        assert_eq!(
            (info.doc_count, info.doc_del_count),
            (ids.len() as u64, 1),
            "{name}"
        );
    }
}
