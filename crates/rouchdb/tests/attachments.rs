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

/// Attach `data` to document `d` (created if needed) and return the digest
/// the database reports for it.
async fn stored_digest(db: &Database, name: &str, data: &[u8], content_type: &str) -> String {
    let rev = match db.get("d").await {
        Ok(doc) => doc.rev.unwrap().to_string(),
        Err(_) => db
            .put("d", serde_json::json!({}))
            .await
            .unwrap()
            .rev
            .unwrap(),
    };
    db.put_attachment("d", name, &rev, data.to_vec(), content_type)
        .await
        .unwrap();
    db.get("d").await.unwrap().attachments[name].digest.clone()
}

/// Attachment digests are `md5-<base64>` of the stored bytes, as in
/// CouchDB 3.5.1: for the same bytes of a type CouchDB stores as is, the
/// local adapters and CouchDB agree. CouchDB gzips compressible types
/// (text/*, application/json, ...) before storing and digests the gzip
/// bytes; rouchdb stores those raw, so their digests differ.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn digests_match_couchdb_for_bytes_stored_as_is() {
    let url = fresh_remote_db("attach_digest").await;
    let remote = Database::http(&url);
    let local = Database::memory("digests");
    let data = b"hello hello hello hello hello hello hello hello";
    // md5 of the bytes, base64-encoded.
    let raw = "md5-7uNimBaMipk1DrT0kixOKA==";
    for (name, content_type) in [
        ("a.bin", "application/octet-stream"),
        ("a.png", "image/png"),
    ] {
        for db in [&remote, &local] {
            let digest = stored_digest(db, name, data, content_type).await;
            assert_eq!(digest, raw, "{content_type}");
        }
    }
    let couch_text = stored_digest(&remote, "a.txt", data, "text/plain").await;
    assert_ne!(couch_text, raw, "CouchDB digests the gzip-encoded body");
    let local_text = stored_digest(&local, "a.txt", data, "text/plain").await;
    assert_eq!(local_text, raw);
}
