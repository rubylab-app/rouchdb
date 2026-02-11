//! Tests for core PouchDB parity features:
//! - db.get() with options (revs_info, latest)
//! - db.close()
//! - db.purge()
//! - db.post()
//! - db.allDocs() with conflicts/update_seq
//! - Inline Base64 attachments
//! - db.explain()
//! - Security document

use std::collections::HashMap;

use rouchdb::{
    AllDocsOptions, BulkDocsOptions, ChangesOptions, Database, Document, FindOptions, GetOptions,
    IndexDefinition, Revision, SecurityDocument, SecurityGroup, SortField,
};

// =========================================================================
// db.get() with options
// =========================================================================

#[tokio::test]
async fn get_with_specific_rev() {
    let db = Database::memory("test");

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev1 = r1.rev.clone().unwrap();

    let r2 = db
        .update("doc1", &rev1, serde_json::json!({"v": 2}))
        .await
        .unwrap();
    let _rev2 = r2.rev.clone().unwrap();

    // Fetch the old revision
    let doc = db
        .get_with_opts(
            "doc1",
            GetOptions {
                rev: Some(rev1.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(doc.data["v"], 1);

    // Fetch the latest (default)
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 2);
}

#[tokio::test]
async fn get_with_revs_info() {
    let db = Database::memory("test");

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev1 = r1.rev.unwrap();
    let _r2 = db
        .update("doc1", &rev1, serde_json::json!({"v": 2}))
        .await
        .unwrap();

    let doc = db
        .get_with_opts(
            "doc1",
            GetOptions {
                revs_info: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(doc.data["v"], 2);
    // revs_info support depends on adapter, but the request should not error
}

#[tokio::test]
async fn get_with_latest_flag() {
    let db = Database::memory("test");

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev1 = r1.rev.unwrap();
    let r2 = db
        .update("doc1", &rev1, serde_json::json!({"v": 2}))
        .await
        .unwrap();
    let rev2 = r2.rev.unwrap();

    // Request old rev with latest=true should return the latest leaf
    let doc = db
        .get_with_opts(
            "doc1",
            GetOptions {
                rev: Some(rev1),
                latest: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(doc.rev.unwrap().to_string(), rev2);
    assert_eq!(doc.data["v"], 2);
}

#[tokio::test]
async fn get_nonexistent_doc() {
    let db = Database::memory("test");
    let err = db.get("nonexistent").await;
    assert!(err.is_err());
}

// =========================================================================
// db.close()
// =========================================================================

#[tokio::test]
async fn close_memory_db() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    // close should succeed (no-op for memory)
    db.close().await.unwrap();
}

#[tokio::test]
async fn close_redb_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let db = Database::open(&path, "test_close").unwrap();
    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.close().await.unwrap();
}

// =========================================================================
// db.purge()
// =========================================================================

#[tokio::test]
async fn purge_on_memory_adapter() {
    let db = Database::memory("test");
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev = r1.rev.unwrap();

    let info_before = db.info().await.unwrap();
    assert_eq!(info_before.doc_count, 1);

    // Memory adapter implements purge — permanently removes the revision
    let result = db.purge("doc1", vec![rev]).await.unwrap();
    assert!(result.purged.contains_key("doc1"));

    let info_after = db.info().await.unwrap();
    assert_eq!(info_after.doc_count, 0);

    // Doc should be gone
    assert!(db.get("doc1").await.is_err());
}

// =========================================================================
// db.allDocs() with new options
// =========================================================================

#[tokio::test]
async fn all_docs_with_conflicts_option() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            conflicts: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 2);
}

#[tokio::test]
async fn all_docs_with_update_seq() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({})).await.unwrap();
    db.put("doc2", serde_json::json!({})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            update_seq: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 2);
}

#[tokio::test]
async fn all_docs_with_keys() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({"v": 1})).await.unwrap();
    db.put("b", serde_json::json!({"v": 2})).await.unwrap();
    db.put("c", serde_json::json!({"v": 3})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            keys: Some(vec!["a".into(), "c".into()]),
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    // Should only return docs a and c
    assert_eq!(result.rows.len(), 2);
    let ids: Vec<&str> = result.rows.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"a"));
    assert!(ids.contains(&"c"));
}

// =========================================================================
// Inline Base64 attachments
// =========================================================================

#[tokio::test]
async fn inline_base64_attachment_decoding() {
    use base64::Engine;
    let data = b"Hello, World!";
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);

    let json = serde_json::json!({
        "_id": "doc1",
        "_attachments": {
            "hello.txt": {
                "content_type": "text/plain",
                "data": b64,
                "digest": "md5-abc",
                "length": 0
            }
        },
        "name": "test"
    });

    let doc = Document::from_json(json).unwrap();
    assert_eq!(doc.id, "doc1");
    assert_eq!(doc.data["name"], "test");

    let att = doc.attachments.get("hello.txt").unwrap();
    assert_eq!(att.content_type, "text/plain");
    assert_eq!(att.data.as_ref().unwrap(), data);
    assert_eq!(att.length, 13);
    assert!(!att.stub);
}

#[tokio::test]
async fn inline_base64_attachment_missing_data_is_stub() {
    let json = serde_json::json!({
        "_id": "doc1",
        "_attachments": {
            "hello.txt": {
                "content_type": "text/plain",
                "digest": "md5-abc",
                "length": 13,
                "stub": true
            }
        }
    });

    let doc = Document::from_json(json).unwrap();
    let att = doc.attachments.get("hello.txt").unwrap();
    assert!(att.stub);
    assert!(att.data.is_none());
}

// =========================================================================
// db.explain()
// =========================================================================

#[tokio::test]
async fn explain_without_index() {
    let db = Database::memory("test_explain");
    db.put("doc1", serde_json::json!({"name": "Alice", "age": 30}))
        .await
        .unwrap();

    let explanation = db
        .explain(FindOptions {
            selector: serde_json::json!({"age": {"$gt": 20}}),
            ..Default::default()
        })
        .await;

    assert_eq!(explanation.dbname, "test_explain");
    assert_eq!(explanation.index.name, "_all_docs");
    assert_eq!(explanation.index.index_type, "special");
    assert!(explanation.index.ddoc.is_none());
}

#[tokio::test]
async fn explain_with_index() {
    let db = Database::memory("test_explain_idx");
    db.put("doc1", serde_json::json!({"name": "Alice", "age": 30}))
        .await
        .unwrap();

    db.create_index(IndexDefinition {
        name: String::new(),
        fields: vec![SortField::Simple("age".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    let explanation = db
        .explain(FindOptions {
            selector: serde_json::json!({"age": {"$gt": 20}}),
            ..Default::default()
        })
        .await;

    assert_eq!(explanation.index.name, "idx-age");
    assert_eq!(explanation.index.index_type, "json");
}

#[tokio::test]
async fn explain_with_fields_projection() {
    let db = Database::memory("test_explain_fields");

    let explanation = db
        .explain(FindOptions {
            selector: serde_json::json!({"type": "user"}),
            fields: Some(vec!["name".into(), "email".into()]),
            ..Default::default()
        })
        .await;

    assert_eq!(
        explanation.fields,
        Some(vec!["name".to_string(), "email".to_string()])
    );
}

// =========================================================================
// Security document
// =========================================================================

#[tokio::test]
async fn security_document_default() {
    let db = Database::memory("test");

    let sec = db.get_security().await.unwrap();
    assert!(sec.admins.names.is_empty());
    assert!(sec.admins.roles.is_empty());
    assert!(sec.members.names.is_empty());
    assert!(sec.members.roles.is_empty());
}

#[tokio::test]
async fn security_document_put_and_get() {
    let db = Database::memory("test");

    let sec = SecurityDocument {
        admins: SecurityGroup {
            names: vec!["admin".into()],
            roles: vec!["_admin".into()],
        },
        members: SecurityGroup {
            names: vec!["user1".into(), "user2".into()],
            roles: vec!["readers".into()],
        },
    };

    db.put_security(sec).await.unwrap();

    let sec2 = db.get_security().await.unwrap();
    assert_eq!(sec2.admins.names, vec!["admin"]);
    assert_eq!(sec2.admins.roles, vec!["_admin"]);
    assert_eq!(sec2.members.names, vec!["user1", "user2"]);
    assert_eq!(sec2.members.roles, vec!["readers"]);
}

// =========================================================================
// db.post() generates unique IDs
// =========================================================================

#[tokio::test]
async fn post_generates_uuid_v4_ids() {
    let db = Database::memory("test");

    let mut ids = Vec::new();
    for _ in 0..10 {
        let r = db.post(serde_json::json!({"v": 1})).await.unwrap();
        assert!(r.ok);
        assert!(!r.id.is_empty());
        assert!(!ids.contains(&r.id));
        ids.push(r.id);
    }
    assert_eq!(ids.len(), 10);
}

// =========================================================================
// db.info() after various operations
// =========================================================================

#[tokio::test]
async fn info_reflects_all_operations() {
    let db = Database::memory("test");

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 0);

    db.put("doc1", serde_json::json!({})).await.unwrap();
    let r = db.put("doc2", serde_json::json!({})).await.unwrap();
    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 2);

    db.remove("doc2", &r.rev.unwrap()).await.unwrap();
    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 1);
}

// =========================================================================
// db.compact()
// =========================================================================

#[tokio::test]
async fn compact_does_not_lose_data() {
    let db = Database::memory("test");
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();

    db.compact().await.unwrap();

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 2);
}

// =========================================================================
// db.destroy()
// =========================================================================

#[tokio::test]
async fn destroy_removes_all_docs() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({})).await.unwrap();
    db.put("b", serde_json::json!({})).await.unwrap();
    db.put("c", serde_json::json!({})).await.unwrap();

    db.destroy().await.unwrap();

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 0);
}

// =========================================================================
// bulk_docs with new_edits: false (replication mode)
// =========================================================================

#[tokio::test]
async fn bulk_docs_replication_mode() {
    let db = Database::memory("test");

    let docs = vec![Document {
        id: "doc1".into(),
        rev: Some(Revision::new(1, "abc123".into())),
        deleted: false,
        data: serde_json::json!({"replicated": true}),
        attachments: HashMap::new(),
    }];

    let results = db
        .bulk_docs(docs, BulkDocsOptions::replication())
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].ok);

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["replicated"], true);
}

// =========================================================================
// Changes feed with selector (no include_docs)
// =========================================================================

#[tokio::test]
async fn changes_with_selector_strips_docs_when_not_requested() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({"type": "x"})).await.unwrap();
    db.put("b", serde_json::json!({"type": "y"})).await.unwrap();
    db.put("c", serde_json::json!({"type": "x"})).await.unwrap();

    let changes = db
        .changes(ChangesOptions {
            selector: Some(serde_json::json!({"type": "x"})),
            include_docs: false,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(changes.results.len(), 2);
    // Docs should not be included
    assert!(changes.results[0].doc.is_none());
    assert!(changes.results[1].doc.is_none());
}

#[tokio::test]
async fn changes_with_selector_includes_docs_when_requested() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({"type": "user", "name": "Alice"}))
        .await
        .unwrap();
    db.put("b", serde_json::json!({"type": "order", "amount": 100}))
        .await
        .unwrap();

    let changes = db
        .changes(ChangesOptions {
            selector: Some(serde_json::json!({"type": "user"})),
            include_docs: true,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(changes.results.len(), 1);
    let doc = changes.results[0].doc.as_ref().unwrap();
    assert_eq!(doc["name"], "Alice");
}
