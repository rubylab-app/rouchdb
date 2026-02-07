//! Integration tests for db.post() and removeAttachment().

mod common;

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::Database;

// -----------------------------------------------------------------------
// db.post() tests
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn post_to_couchdb() {
    let url = fresh_remote_db("post").await;
    let db = Database::http(&url);

    let r1 = db
        .post(serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();
    assert!(r1.ok);
    assert!(!r1.id.is_empty());

    let r2 = db
        .post(serde_json::json!({"name": "Bob"}))
        .await
        .unwrap();
    assert!(r2.ok);
    assert_ne!(r1.id, r2.id);

    let doc = db.get(&r1.id).await.unwrap();
    assert_eq!(doc.data["name"], "Alice");

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
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

    remote.get(&r1.id).await.unwrap();
    remote.get(&r2.id).await.unwrap();

    delete_remote_db(&url).await;
}

// -----------------------------------------------------------------------
// removeAttachment() tests
// -----------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn remove_attachment_from_couchdb() {
    let url = fresh_remote_db("rm_att").await;
    let db = Database::http(&url);

    // Create a doc and add an attachment
    let r1 = db
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    let rev1 = r1.rev.unwrap();

    let att_result = db
        .adapter()
        .put_attachment("doc1", "hello.txt", &rev1, b"Hello, World!".to_vec(), "text/plain")
        .await
        .unwrap();
    let rev2 = att_result.rev.unwrap();

    // Verify attachment exists
    let att_data = db
        .adapter()
        .get_attachment("doc1", "hello.txt", rouchdb::GetAttachmentOptions { rev: None })
        .await
        .unwrap();
    assert_eq!(att_data, b"Hello, World!");

    // Remove the attachment
    let rm_result = db.remove_attachment("doc1", "hello.txt", &rev2).await.unwrap();
    assert!(rm_result.ok);

    // Verify attachment is gone
    let err = db
        .adapter()
        .get_attachment("doc1", "hello.txt", rouchdb::GetAttachmentOptions { rev: None })
        .await;
    assert!(err.is_err());

    delete_remote_db(&url).await;
}
