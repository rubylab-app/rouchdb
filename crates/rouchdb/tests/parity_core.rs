//! Tests for core PouchDB parity features of the `Database` API, on the
//! memory and the redb backend:
//! - db.get() with a specific rev, tombstones
//! - db.close()
//! - db.post()
//! - db.allDocs() with conflicts/update_seq
//! - Inline Base64 attachments and the document JSON shape
//! - db.explain()
//! - Security document
//! - db.destroy()
//! - Changes filtered by a Mango selector

mod backends;

use std::collections::HashMap;

use backends::{Backend, KINDS, backends, row_ids};
use rouchdb::{
    AllDocsOptions, AttachmentMeta, BulkDocsOptions, ChangesOptions, Database, Document,
    FindOptions, GetOptions, IndexDefinition, Revision, RouchError, SecurityDocument,
    SecurityGroup, Seq, SortField,
};

fn doc(json: serde_json::Value) -> Document {
    Document::from_json(json).unwrap()
}

// =========================================================================
// db.get() with options
// =========================================================================

#[tokio::test]
async fn get_with_specific_rev() {
    for b in backends("test") {
        let db = &b.db;
        let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let rev1 = r1.rev.clone().unwrap();
        let r2 = db
            .update("doc1", &rev1, serde_json::json!({"v": 2}))
            .await
            .unwrap();

        // Fetch the old revision
        let old = db
            .get_with_opts(
                "doc1",
                GetOptions {
                    rev: Some(rev1.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(old.rev.unwrap().to_string(), rev1, "{}", b.name);
        assert_eq!(old.data, serde_json::json!({"v": 1}), "{}", b.name);

        // Fetch the latest (default)
        let latest = db.get("doc1").await.unwrap();
        assert_eq!(latest.rev.unwrap().to_string(), r2.rev.unwrap());
        assert_eq!(latest.data, serde_json::json!({"v": 2}), "{}", b.name);
    }
}

/// Like CouchDB, the tombstone written by a delete has no body: reading it
/// by revision returns only `_id`, `_rev` and `_deleted`.
#[tokio::test]
async fn removed_document_tombstone_has_no_body() {
    for b in backends("test") {
        let db = &b.db;
        let r1 = db
            .put("doc1", serde_json::json!({"name": "Alice", "tags": ["a"]}))
            .await
            .unwrap();
        let tomb = db.remove("doc1", &r1.rev.unwrap()).await.unwrap();
        let tomb_rev = tomb.rev.unwrap();
        assert!(tomb_rev.starts_with("2-"), "{}: {tomb_rev}", b.name);

        let got = db
            .get_with_opts(
                "doc1",
                GetOptions {
                    rev: Some(tomb_rev.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(got.deleted, "{}", b.name);
        assert_eq!(
            got.to_json(),
            serde_json::json!({"_id": "doc1", "_rev": tomb_rev, "_deleted": true}),
            "{}",
            b.name
        );
        assert!(
            matches!(db.get("doc1").await, Err(RouchError::NotFound(_))),
            "{}",
            b.name
        );
    }
}

// =========================================================================
// db.close()
// =========================================================================

#[tokio::test]
async fn close_redb_db_keeps_the_data_for_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let (kept, updated, removed) = {
        let db = Database::open(&path, "test_close").unwrap();
        let kept = db.put("kept", serde_json::json!({"v": 1})).await.unwrap();
        let r = db
            .put("updated", serde_json::json!({"v": 1}))
            .await
            .unwrap();
        let updated = db
            .update("updated", &r.rev.unwrap(), serde_json::json!({"v": 2}))
            .await
            .unwrap();
        let r = db
            .put("removed", serde_json::json!({"v": 1}))
            .await
            .unwrap();
        let removed = db.remove("removed", &r.rev.unwrap()).await.unwrap();
        db.close().await.unwrap();
        (
            kept.rev.unwrap(),
            updated.rev.unwrap(),
            removed.rev.unwrap(),
        )
    };

    let db = Database::open(&path, "test_close").unwrap();
    let got = db.get("kept").await.unwrap();
    assert_eq!(
        (got.rev.unwrap().to_string(), got.data),
        (kept, serde_json::json!({"v": 1}))
    );
    let got = db.get("updated").await.unwrap();
    assert_eq!(
        (got.rev.unwrap().to_string(), got.data),
        (updated, serde_json::json!({"v": 2}))
    );
    assert!(matches!(
        db.get("removed").await,
        Err(RouchError::NotFound(_))
    ));
    let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(row_ids(&all), ["kept", "updated"]);
    let info = db.info().await.unwrap();
    assert_eq!(
        (info.doc_count, info.doc_del_count, info.update_seq),
        (2, 1, Seq::Num(5))
    );
    // The tombstone is still there to replicate.
    let changes = db.changes(ChangesOptions::default()).await.unwrap();
    let removed_change = changes.results.iter().find(|c| c.id == "removed").unwrap();
    assert!(removed_change.deleted);
    assert_eq!(removed_change.changes[0].rev, removed);
}

// =========================================================================
// db.allDocs() with conflicts / update_seq
// =========================================================================

/// `doc1` with two live leaves (the replicated `2-fff…` wins over the
/// local `2-…`), and a plain `doc2`. Returns (winner, loser).
async fn conflicted(db: &Database) -> (String, String) {
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let r1 = r1.rev.unwrap();
    let local = db
        .update("doc1", &r1, serde_json::json!({"v": "local"}))
        .await
        .unwrap()
        .rev
        .unwrap();
    let hash = "f".repeat(32);
    let winner = format!("2-{hash}");
    let results = db
        .bulk_docs(
            vec![doc(serde_json::json!({
                "_id": "doc1",
                "_rev": winner,
                "v": "remote",
                "_revisions": {"start": 2, "ids": [hash, r1.split_once('-').unwrap().1]},
            }))],
            BulkDocsOptions::replication(),
        )
        .await
        .unwrap();
    assert!(results.iter().all(|r| r.ok), "{results:?}");
    db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();
    // Precondition: the replicated revision wins.
    assert_eq!(
        db.get("doc1").await.unwrap().rev.unwrap().to_string(),
        winner
    );
    (winner, local)
}

#[tokio::test]
async fn all_docs_conflicts_lists_losing_revisions() {
    for b in backends("test") {
        let db = &b.db;
        let (winner, loser) = conflicted(db).await;
        let result = db
            .all_docs(AllDocsOptions {
                include_docs: true,
                conflicts: true,
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        assert_eq!(row_ids(&result), ["doc1", "doc2"], "{}", b.name);
        assert_eq!(result.rows[0].value.rev, winner, "{}", b.name);
        let doc1 = result.rows[0].doc.as_ref().unwrap();
        assert_eq!(doc1["v"], "remote", "{}", b.name);
        assert_eq!(doc1["_conflicts"], serde_json::json!([loser]), "{}", b.name);
        let doc2 = result.rows[1].doc.as_ref().unwrap();
        assert!(doc2.get("_conflicts").is_none(), "{}: {doc2}", b.name);

        // Without `conflicts` the documents carry no `_conflicts`.
        let plain = db
            .all_docs(AllDocsOptions {
                include_docs: true,
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        let doc1 = plain.rows[0].doc.as_ref().unwrap();
        assert!(doc1.get("_conflicts").is_none(), "{}: {doc1}", b.name);
    }
}

#[tokio::test]
async fn all_docs_update_seq_is_the_database_sequence() {
    for b in backends("test") {
        let db = &b.db;
        db.put("doc1", serde_json::json!({})).await.unwrap();
        let r = db.put("doc2", serde_json::json!({})).await.unwrap();
        db.remove("doc2", &r.rev.unwrap()).await.unwrap();

        let with_seq = db
            .all_docs(AllDocsOptions {
                update_seq: true,
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        assert_eq!(row_ids(&with_seq), ["doc1"], "{}", b.name);
        assert_eq!(with_seq.update_seq, Some(Seq::Num(3)), "{}", b.name);
        assert_eq!(
            with_seq.update_seq,
            Some(db.info().await.unwrap().update_seq),
            "{}",
            b.name
        );

        let without = db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert_eq!(without.update_seq, None, "{}", b.name);
    }
}

// =========================================================================
// Inline Base64 attachments and the document JSON shape
// =========================================================================

// Fixtures captured from CouchDB 3.5.1 for the document
// `{"_id": "doc1", "name": "test"}` with the attachment `hello.bin`
// (`application/octet-stream`, content "Hello, World!").
const HELLO: &[u8] = b"Hello, World!";
const HELLO_B64: &str = "SGVsbG8sIFdvcmxkIQ==";
const HELLO_DIGEST: &str = "md5-ZajifYh5KDgxtmS9i38K1A==";

fn hello(attachment: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "_id": "doc1",
        "_rev": "1-d13178e2b436fa29621d6329b3a5f83b",
        "name": "test",
        "_attachments": {"hello.bin": attachment}
    })
}

/// The attachment shapes CouchDB and PouchDB exchange decode to the same
/// metadata CouchDB reports (the digest is CouchDB's own).
#[tokio::test]
async fn attachment_json_shapes_decode_like_couchdb() {
    let content_type = "application/octet-stream";
    // (JSON shape, expected data, expected stub)
    let shapes = [
        // Written by a client: only content_type and base64 data; the
        // digest and length are computed.
        (
            serde_json::json!({"content_type": content_type, "data": HELLO_B64}),
            Some(HELLO),
            false,
        ),
        // `GET /db/doc1?attachments=true`: inline data, digest and revpos,
        // but no length.
        (
            serde_json::json!({
                "content_type": content_type, "revpos": 1,
                "digest": HELLO_DIGEST, "data": HELLO_B64
            }),
            Some(HELLO),
            false,
        ),
        // Plain `GET /db/doc1`: a stub.
        (
            serde_json::json!({
                "content_type": content_type, "revpos": 1,
                "digest": HELLO_DIGEST, "length": 13, "stub": true
            }),
            None,
            true,
        ),
    ];
    for (shape, data, stub) in shapes {
        let parsed = Document::from_json(hello(shape.clone())).unwrap();
        assert_eq!(parsed.id, "doc1");
        assert_eq!(parsed.data, serde_json::json!({"name": "test"}), "{shape}");
        assert_eq!(parsed.attachments.len(), 1, "{shape}");
        let att = &parsed.attachments["hello.bin"];
        assert_eq!(att.content_type, content_type, "{shape}");
        assert_eq!(att.digest, HELLO_DIGEST, "{shape}");
        assert_eq!(att.length, 13, "{shape}");
        assert_eq!(att.stub, stub, "{shape}");
        assert_eq!(att.data.as_deref(), data, "{shape}");
    }
}

/// `to_json` and `from_json` are inverse: nothing is lost or added.
#[tokio::test]
async fn document_json_roundtrip_is_lossless() {
    use base64::Engine;
    // Sanity-check the captured fixture itself.
    assert_eq!(
        base64::engine::general_purpose::STANDARD.encode(HELLO),
        HELLO_B64
    );

    let meta = |stub: bool| AttachmentMeta {
        content_type: "application/octet-stream".into(),
        digest: HELLO_DIGEST.into(),
        length: HELLO.len() as u64,
        stub,
        data: (!stub).then(|| HELLO.to_vec()),
    };
    let docs = [
        Document {
            id: "inline".into(),
            rev: Some(Revision::new(1, "abc".into())),
            deleted: false,
            data: serde_json::json!({"name": "Alice", "nested": {"a": [1, {"b": null}]}}),
            attachments: HashMap::from([("hello.bin".to_string(), meta(false))]),
        },
        Document {
            id: "stub".into(),
            rev: Some(Revision::new(3, "def".into())),
            deleted: false,
            data: serde_json::json!({"n": 1.5, "s": "", "e": {}}),
            attachments: HashMap::from([("hello.bin".to_string(), meta(true))]),
        },
        Document {
            id: "tomb".into(),
            rev: Some(Revision::new(2, "fed".into())),
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        },
    ];
    for original in docs {
        let json = original.to_json();
        let back = Document::from_json(json.clone()).unwrap();
        assert_eq!(back.to_json(), json);
        assert_eq!(back.id, original.id);
        assert_eq!(back.rev, original.rev);
        assert_eq!(back.deleted, original.deleted);
        assert_eq!(back.data, original.data, "{json}");
        for (name, att) in &original.attachments {
            let got = &back.attachments[name];
            assert_eq!(
                (
                    &got.content_type,
                    &got.digest,
                    got.length,
                    got.stub,
                    &got.data
                ),
                (
                    &att.content_type,
                    &att.digest,
                    att.length,
                    att.stub,
                    &att.data
                ),
                "{json}"
            );
        }
    }

    // The inline form is base64, and a live document has no `_deleted`.
    let json = Document::from_json(hello(
        serde_json::json!({"content_type": "application/octet-stream", "data": HELLO_B64}),
    ))
    .unwrap()
    .to_json();
    assert_eq!(json["_attachments"]["hello.bin"]["data"], HELLO_B64);
    assert!(json.get("_deleted").is_none(), "{json}");

    // An empty `_attachments` object is no attachment at all.
    let empty = Document::from_json(serde_json::json!({"_id": "d", "_attachments": {}})).unwrap();
    assert!(empty.attachments.is_empty());
    assert!(empty.to_json().get("_attachments").is_none());

    // A malformed `_rev` is rejected as such.
    assert!(matches!(
        Document::from_json(serde_json::json!({"_id": "d", "_rev": "not-a-valid-rev"})),
        Err(RouchError::InvalidRev(_))
    ));
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
    assert!(explanation.index.def.fields.is_empty());
    assert_eq!(
        explanation.selector,
        serde_json::json!({"age": {"$gt": 20}})
    );
}

/// The index used is one whose first field the selector constrains (the
/// smallest name when several are); others fall back to `_all_docs`.
#[tokio::test]
async fn explain_picks_the_index_on_the_selector_field() {
    let db = Database::memory("test_explain_idx");
    db.put(
        "doc1",
        serde_json::json!({"name": "A", "age": 1, "city": "NYC"}),
    )
    .await
    .unwrap();
    for (name, fields) in [
        ("", vec!["name"]),
        ("", vec!["age"]),
        ("by-city-age", vec!["city", "age"]),
        ("z-age", vec!["age"]),
    ] {
        db.create_index(IndexDefinition {
            name: name.into(),
            fields: fields
                .into_iter()
                .map(|f| SortField::Simple(f.into()))
                .collect(),
            ddoc: None,
        })
        .await
        .unwrap();
    }

    let explain = |selector: serde_json::Value| {
        let db = &db;
        async move {
            db.explain(FindOptions {
                selector,
                ..Default::default()
            })
            .await
        }
    };
    for (selector, index, fields) in [
        (
            serde_json::json!({"age": {"$gte": 0}}),
            "idx-age",
            vec!["age"],
        ),
        (serde_json::json!({"name": "A"}), "idx-name", vec!["name"]),
        (
            serde_json::json!({"city": "NYC"}),
            "by-city-age",
            vec!["city", "age"],
        ),
        // Several usable indexes: the smallest name.
        (
            serde_json::json!({"city": "NYC", "age": 1}),
            "by-city-age",
            vec!["city", "age"],
        ),
        (
            serde_json::json!({"name": "A", "age": 1}),
            "idx-age",
            vec!["age"],
        ),
    ] {
        let explanation = explain(selector.clone()).await;
        assert_eq!(explanation.dbname, "test_explain_idx");
        assert_eq!(explanation.index.name, index, "{selector}");
        assert_eq!(explanation.index.index_type, "json", "{selector}");
        assert_eq!(explanation.index.ddoc, None, "{selector}");
        let got: Vec<String> = explanation
            .index
            .def
            .fields
            .iter()
            .map(|f| f.field_and_direction().0.to_string())
            .collect();
        assert_eq!(got, fields, "{selector}");
        assert_eq!(explanation.selector, selector);
    }

    // No index on the selector's fields.
    let explanation = explain(serde_json::json!({"other": 1})).await;
    assert_eq!(explanation.index.name, "_all_docs");
    assert_eq!(explanation.index.index_type, "special");
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
async fn security_document_roundtrip_and_overwrite() {
    for b in backends("test") {
        let db = &b.db;
        let as_json = |sec: &SecurityDocument| serde_json::to_value(sec).unwrap();

        // No security document yet: nobody is listed.
        let empty = serde_json::json!({
            "admins": {"names": [], "roles": []},
            "members": {"names": [], "roles": []}
        });
        assert_eq!(
            as_json(&db.get_security().await.unwrap()),
            empty,
            "{}",
            b.name
        );

        let mut sec = SecurityDocument {
            admins: SecurityGroup {
                names: vec!["admin".into()],
                roles: vec!["_admin".into()],
            },
            members: SecurityGroup {
                names: vec!["user1".into(), "user2".into()],
                roles: vec!["readers".into()],
            },
            ..Default::default()
        };
        sec.extra
            .insert("custom".into(), serde_json::json!({"k": [1]}));
        db.put_security(sec.clone()).await.unwrap();
        assert_eq!(
            as_json(&db.get_security().await.unwrap()),
            as_json(&sec),
            "{}",
            b.name
        );

        // A new document replaces the old one wholesale.
        let replacement = SecurityDocument {
            admins: SecurityGroup {
                names: vec!["admin2".into()],
                roles: vec![],
            },
            ..Default::default()
        };
        db.put_security(replacement.clone()).await.unwrap();
        assert_eq!(
            as_json(&db.get_security().await.unwrap()),
            as_json(&replacement),
            "{}",
            b.name
        );
    }
}

// =========================================================================
// db.post()
// =========================================================================

#[tokio::test]
async fn post_generates_uuid_v4_ids() {
    for b in backends("test") {
        let db = &b.db;
        let mut ids = Vec::new();
        for i in 0..10 {
            let r = db.post(serde_json::json!({"i": i})).await.unwrap();
            assert!(r.ok);
            let uuid = uuid::Uuid::parse_str(&r.id)
                .unwrap_or_else(|e| panic!("{}: {} is not a UUID: {e}", b.name, r.id));
            assert_eq!(uuid.get_version_num(), 4, "{}: {}", b.name, r.id);
            assert!(!ids.contains(&r.id), "{}: duplicate id {}", b.name, r.id);
            assert_eq!(db.get(&r.id).await.unwrap().data["i"], i);
            ids.push(r.id);
        }
        let mut all = row_ids(&db.all_docs(AllDocsOptions::new()).await.unwrap());
        all.sort();
        ids.sort();
        assert_eq!(all, ids, "{}", b.name);
    }
}

#[tokio::test]
async fn posted_documents_sync_under_their_ids() {
    for kind in KINDS {
        let a = Backend::open(kind, "a");
        let b = Backend::open(kind, "b");
        let ra = a.db.post(serde_json::json!({"from": "a"})).await.unwrap();
        let rb = b.db.post(serde_json::json!({"from": "b"})).await.unwrap();
        a.db.sync(&b.db).await.unwrap();

        let mut expected = vec![ra.id.clone(), rb.id.clone()];
        expected.sort();
        for (db, name) in [(&a.db, "a"), (&b.db, "b")] {
            let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
            assert_eq!(row_ids(&all), expected, "{kind} {name}");
            for (r, from) in [(&ra, "a"), (&rb, "b")] {
                let got = db.get(&r.id).await.unwrap();
                assert_eq!(got.rev.unwrap().to_string(), *r.rev.as_ref().unwrap());
                assert_eq!(got.data, serde_json::json!({"from": from}), "{kind} {name}");
            }
        }
    }
}

// =========================================================================
// db.destroy()
// =========================================================================

#[tokio::test]
async fn destroy_removes_docs_local_docs_and_indexes() {
    for b in backends("test") {
        let db = &b.db;
        for id in ["a", "b", "c"] {
            db.put(id, serde_json::json!({"n": id})).await.unwrap();
        }
        db.adapter()
            .put_local("checkpoint", serde_json::json!({"seq": 3}))
            .await
            .unwrap();
        db.create_index(IndexDefinition {
            name: String::new(),
            fields: vec![SortField::Simple("n".into())],
            ddoc: None,
        })
        .await
        .unwrap();

        db.destroy().await.unwrap();

        let info = db.info().await.unwrap();
        assert_eq!(
            (info.doc_count, info.doc_del_count, info.update_seq),
            (0, 0, Seq::Num(0)),
            "{}",
            b.name
        );
        assert!(
            db.all_docs(AllDocsOptions::new())
                .await
                .unwrap()
                .rows
                .is_empty()
        );
        let changes = db.changes(ChangesOptions::default()).await.unwrap();
        assert!(changes.results.is_empty(), "{}", b.name);
        assert!(
            matches!(db.get("a").await, Err(RouchError::NotFound(_))),
            "{}",
            b.name
        );
        assert!(
            matches!(
                db.adapter().get_local("checkpoint").await,
                Err(RouchError::NotFound(_))
            ),
            "{}",
            b.name
        );
        assert!(db.get_indexes().await.is_empty(), "{}", b.name);

        // The database can be used again from scratch.
        let r = db.put("a", serde_json::json!({"n": "new"})).await.unwrap();
        assert!(r.rev.unwrap().starts_with("1-"), "{}", b.name);
    }
}

// =========================================================================
// bulk_docs with new_edits: false (replication mode)
// =========================================================================

#[tokio::test]
async fn bulk_docs_replication_mode_keeps_the_given_revision() {
    for b in backends("test") {
        let db = &b.db;
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
        assert!(results[0].ok, "{}: {:?}", b.name, results[0]);

        let got = db.get("doc1").await.unwrap();
        assert_eq!(got.rev.unwrap().to_string(), "1-abc123", "{}", b.name);
        assert_eq!(got.data, serde_json::json!({"replicated": true}));
    }
}

// =========================================================================
// Changes feed filtered by a Mango selector
// =========================================================================

/// Like CouchDB's `filter=_selector`, only the changes whose current
/// document matches are returned (a tombstone matches only a selector on
/// `_deleted`), and `last_seq` is the database's sequence.
#[tokio::test]
async fn changes_with_selector_returns_the_matching_changes() {
    for b in backends("test") {
        let db = &b.db;
        db.put("alice", serde_json::json!({"type": "user", "age": 30}))
            .await
            .unwrap(); // seq 1
        let inv = db
            .put("inv1", serde_json::json!({"type": "invoice"}))
            .await
            .unwrap(); // seq 2
        db.put("bob", serde_json::json!({"type": "user", "age": 25}))
            .await
            .unwrap(); // seq 3
        let carol = db
            .put("carol", serde_json::json!({"type": "user"}))
            .await
            .unwrap(); // seq 4
        db.remove("carol", &carol.rev.unwrap()).await.unwrap(); // seq 5
        let inv2 = db
            .update(
                "inv1",
                &inv.rev.unwrap(),
                serde_json::json!({"type": "user"}),
            )
            .await
            .unwrap(); // seq 6

        let query = |selector: serde_json::Value, since: u64, include_docs: bool| ChangesOptions {
            selector: Some(selector),
            since: Seq::Num(since),
            include_docs,
            ..Default::default()
        };
        let users = serde_json::json!({"type": "user"});

        let changes = db.changes(query(users.clone(), 0, false)).await.unwrap();
        let got: Vec<(&str, u64)> = changes
            .results
            .iter()
            .map(|c| (c.id.as_str(), c.seq.as_num()))
            .collect();
        assert_eq!(got, [("alice", 1), ("bob", 3), ("inv1", 6)], "{}", b.name);
        assert_eq!(changes.last_seq, Seq::Num(6), "{}", b.name);
        assert!(
            changes.results.iter().all(|c| c.doc.is_none()),
            "{}",
            b.name
        );
        assert_eq!(
            changes.results[2].changes[0].rev,
            *inv2.rev.as_ref().unwrap()
        );

        let with_docs = db.changes(query(users.clone(), 0, true)).await.unwrap();
        let docs: Vec<&serde_json::Value> = with_docs
            .results
            .iter()
            .map(|c| c.doc.as_ref().unwrap())
            .collect();
        assert_eq!(docs[0]["age"], 30, "{}", b.name);
        assert_eq!(docs[2]["_rev"], *inv2.rev.as_ref().unwrap(), "{}", b.name);
        assert_eq!(docs[2]["type"], "user", "{}", b.name);

        let since = db.changes(query(users, 3, false)).await.unwrap();
        let ids: Vec<&str> = since.results.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["inv1"], "{}", b.name);
        assert_eq!(since.last_seq, Seq::Num(6), "{}", b.name);

        // Nothing matches: no results, but the feed still moves on.
        let none = db
            .changes(query(serde_json::json!({"type": "nothing"}), 0, false))
            .await
            .unwrap();
        assert!(none.results.is_empty(), "{}", b.name);
        assert_eq!(none.last_seq, Seq::Num(6), "{}", b.name);

        // A tombstone matches a selector on `_deleted`.
        let deleted = db
            .changes(query(serde_json::json!({"_deleted": true}), 0, false))
            .await
            .unwrap();
        let got: Vec<(&str, bool)> = deleted
            .results
            .iter()
            .map(|c| (c.id.as_str(), c.deleted))
            .collect();
        assert_eq!(got, [("carol", true)], "{}", b.name);

        let invalid = db
            .changes(query(serde_json::json!({"type": {"$bogus": 1}}), 0, false))
            .await;
        assert!(
            matches!(invalid, Err(RouchError::BadRequest(_))),
            "{}: {invalid:?}",
            b.name
        );
    }
}
