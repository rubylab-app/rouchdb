//! Integration tests for db.post() and removeAttachment() against CouchDB.

mod common;

use common::fresh_remote_db;
use rouchdb::{AllDocsOptions, Database, DocResult, RouchError};

fn assert_uuid_v4(id: &str) {
    let uuid = uuid::Uuid::parse_str(id).unwrap_or_else(|e| panic!("{id} is not a UUID: {e}"));
    assert_eq!(uuid.get_version_num(), 4, "{id}");
    // 32 hex digits, like the ids CouchDB generates.
    assert_eq!(id, uuid.simple().to_string());
}

/// The ids of all documents, sorted.
async fn all_ids(db: &Database) -> Vec<String> {
    let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
    all.rows.into_iter().map(|r| r.id).collect()
}

fn sorted_ids(results: &[&DocResult]) -> Vec<String> {
    let mut ids: Vec<String> = results.iter().map(|r| r.id.clone()).collect();
    ids.sort();
    ids
}

// -----------------------------------------------------------------------
// db.post() tests
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn post_to_couchdb() {
    let url = fresh_remote_db("post").await;
    let db = Database::http(&url);

    let r1 = db.post(serde_json::json!({"name": "Alice"})).await.unwrap();
    assert!(r1.ok);
    assert_uuid_v4(&r1.id);

    let r2 = db.post(serde_json::json!({"name": "Bob"})).await.unwrap();
    assert!(r2.ok);
    assert_uuid_v4(&r2.id);
    assert_ne!(r1.id, r2.id);

    let doc = db.get(&r1.id).await.unwrap();
    assert_eq!(doc.rev.unwrap().to_string(), r1.rev.clone().unwrap());
    assert_eq!(doc.data, serde_json::json!({"name": "Alice"}));

    assert_eq!(all_ids(&db).await, sorted_ids(&[&r1, &r2]));
}

/// Documents without an id get one from CouchDB over http, as the local
/// adapters generate one: 32 hex digits either way.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn bulk_docs_without_ids_over_http() {
    let url = fresh_remote_db("post_bulk").await;
    let db = Database::http(&url);
    let docs = vec![
        rouchdb::Document::from_json(serde_json::json!({"n": 1})).unwrap(),
        rouchdb::Document::from_json(serde_json::json!({"_id": "named", "n": 2})).unwrap(),
    ];
    let results = db
        .bulk_docs(docs, rouchdb::BulkDocsOptions::new())
        .await
        .unwrap();
    assert!(results.iter().all(|r| r.ok), "{results:?}");
    let generated = &results[0].id;
    assert_eq!(generated.len(), 32, "{generated}");
    assert!(
        generated
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{generated}"
    );
    assert_eq!(results[1].id, "named");
    assert_eq!(db.get(generated).await.unwrap().data["n"], 1);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn post_and_replicate_to_couchdb() {
    let url = fresh_remote_db("post_repl").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .post(serde_json::json!({"type": "note", "title": "Hello"}))
        .await
        .unwrap();
    let r2 = local
        .post(serde_json::json!({"type": "note", "title": "World"}))
        .await
        .unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 2);

    // The target has exactly the posted documents, under the same ids and
    // revisions.
    assert_eq!(all_ids(&remote).await, sorted_ids(&[&r1, &r2]));
    for (r, title) in [(&r1, "Hello"), (&r2, "World")] {
        let doc = remote.get(&r.id).await.unwrap();
        assert_eq!(doc.rev.unwrap().to_string(), r.rev.clone().unwrap());
        assert_eq!(
            doc.data,
            serde_json::json!({"type": "note", "title": title})
        );
    }
}

// -----------------------------------------------------------------------
// removeAttachment() tests
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn remove_attachment_from_couchdb() {
    let url = fresh_remote_db("rm_att").await;
    let db = Database::http(&url);

    // Create a doc and add two attachments
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev2 = db
        .put_attachment(
            "doc1",
            "hello.txt",
            &r1.rev.unwrap(),
            b"Hello, World!".to_vec(),
            "text/plain",
        )
        .await
        .unwrap()
        .rev
        .unwrap();
    let rev3 = db
        .put_attachment("doc1", "keep.txt", &rev2, b"keep".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();

    // Verify attachment exists
    assert_eq!(
        db.get_attachment("doc1", "hello.txt").await.unwrap(),
        b"Hello, World!"
    );

    // Remove one attachment
    let rm_result = db
        .remove_attachment("doc1", "hello.txt", &rev3)
        .await
        .unwrap();
    assert!(rm_result.ok);
    let rev4 = rm_result.rev.unwrap();
    assert!(rev4.starts_with("4-"), "{rev4}");

    // It is gone; the other attachment and the body stay.
    let err = db.get_attachment("doc1", "hello.txt").await;
    assert!(matches!(err, Err(RouchError::NotFound(_))), "{err:?}");
    assert_eq!(
        db.get_attachment("doc1", "keep.txt").await.unwrap(),
        b"keep"
    );
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.rev.unwrap().to_string(), rev4);
    assert_eq!(doc.data, serde_json::json!({"v": 1}));
    let names: Vec<&String> = doc.attachments.keys().collect();
    assert_eq!(names, ["keep.txt"]);

    // Removing it again is not found.
    let again = db.remove_attachment("doc1", "hello.txt", &rev4).await;
    assert!(matches!(again, Err(RouchError::NotFound(_))), "{again:?}");
}
