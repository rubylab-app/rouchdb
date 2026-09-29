//! Attachment tests: put/get text and binary data on CouchDB through the
//! `Database` API.

mod common;

use common::fresh_remote_db;
use rouchdb::{Database, GetAttachmentOptions, RouchError};

/// Stores `data` as `name` on a new document and checks what CouchDB then
/// reports for it.
async fn roundtrip(db: &Database, name: &str, data: &[u8], content_type: &str) {
    let r1 = db
        .put("doc1", serde_json::json!({"name": "test"}))
        .await
        .unwrap();
    let r1 = r1.rev.unwrap();

    let result = db
        .put_attachment("doc1", name, &r1, data.to_vec(), content_type)
        .await
        .unwrap();
    assert!(result.ok);
    let r2 = result.rev.unwrap();
    assert!(r2.starts_with("2-"), "{r2}");

    assert_eq!(db.get_attachment("doc1", name).await.unwrap(), data);
    // The attachment belongs to the new revision only.
    let at_r1 = db
        .get_attachment_with_opts("doc1", name, GetAttachmentOptions { rev: Some(r1) })
        .await;
    assert!(matches!(at_r1, Err(RouchError::NotFound(_))), "{at_r1:?}");
    let at_r2 = db
        .get_attachment_with_opts(
            "doc1",
            name,
            GetAttachmentOptions {
                rev: Some(r2.clone()),
            },
        )
        .await
        .unwrap();
    assert_eq!(at_r2, data);

    // The document keeps its body and lists the attachment as a stub.
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.rev.unwrap().to_string(), r2);
    assert_eq!(doc.data, serde_json::json!({"name": "test"}));
    assert_eq!(doc.attachments.len(), 1);
    let meta = &doc.attachments[name];
    assert_eq!(meta.content_type, content_type);
    assert_eq!(meta.length, data.len() as u64);
    assert!(meta.stub);
    assert!(meta.data.is_none());
    assert!(meta.digest.starts_with("md5-"), "{}", meta.digest);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn attachment_put_and_get_http() {
    let url = fresh_remote_db("attach").await;
    let db = Database::http(&url);
    roundtrip(
        &db,
        "greeting.txt",
        b"Hello, CouchDB attachments!",
        "text/plain",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn attachment_binary_data() {
    let url = fresh_remote_db("attach_bin").await;
    let db = Database::http(&url);
    let binary_data: Vec<u8> = (0..=255).collect();
    roundtrip(&db, "bytes.bin", &binary_data, "application/octet-stream").await;
}
