//! Attachment tests: put/get text and binary data via HTTP adapter.

mod common;

use common::fresh_remote_db;
use rouchdb::{Database, GetAttachmentOptions};

#[tokio::test]
#[ignore]
async fn attachment_put_and_get_http() {
    let url = fresh_remote_db("attach").await;
    let db = Database::http(&url);

    let r1 = db
        .put("doc1", serde_json::json!({"name": "test"}))
        .await
        .unwrap();
    let rev = r1.rev.unwrap();

    let data = b"Hello, CouchDB attachments!".to_vec();
    let result = db
        .adapter()
        .put_attachment("doc1", "greeting.txt", &rev, data.clone(), "text/plain")
        .await
        .unwrap();
    assert!(result.ok);

    let retrieved = db
        .adapter()
        .get_attachment("doc1", "greeting.txt", GetAttachmentOptions::default())
        .await
        .unwrap();
    assert_eq!(retrieved, data);

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["name"], "test");
}

#[tokio::test]
#[ignore]
async fn attachment_binary_data() {
    let url = fresh_remote_db("attach_bin").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({})).await.unwrap();
    let rev = r1.rev.unwrap();

    let binary_data: Vec<u8> = (0..=255).collect();
    let result = db
        .adapter()
        .put_attachment(
            "doc1",
            "bytes.bin",
            &rev,
            binary_data.clone(),
            "application/octet-stream",
        )
        .await
        .unwrap();
    assert!(result.ok);

    let retrieved = db
        .adapter()
        .get_attachment("doc1", "bytes.bin", GetAttachmentOptions::default())
        .await
        .unwrap();
    assert_eq!(retrieved, binary_data);
}
