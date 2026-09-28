//! Cross-adapter conformance suite.
//!
//! Every scenario runs against the in-memory adapter AND the redb adapter,
//! so a write path that behaves differently on one backend (a response that
//! says `ok:true` but did not store the data, an option ignored by one
//! adapter, ...) fails CI instead of shipping.
//!
//! Add new scenarios as `async fn name(fx: Fx)` and register them in a
//! `conformance!` block; the macro generates one test per adapter.

use rouchdb::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Memory,
    Redb,
}

/// A database under test, plus what is needed to close and reopen it.
struct Fx {
    kind: Kind,
    dir: tempfile::TempDir,
    db: Option<Database>,
}

impl Fx {
    fn new(kind: Kind) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = match kind {
            Kind::Memory => Database::memory("conformance"),
            Kind::Redb => Database::open(dir.path().join("db.redb"), "conformance").unwrap(),
        };
        Fx {
            kind,
            dir,
            db: Some(db),
        }
    }

    /// A second, independent database of the same kind.
    fn sibling(&self, name: &str) -> Database {
        match self.kind {
            Kind::Memory => Database::memory(name),
            Kind::Redb => {
                Database::open(self.dir.path().join(format!("{}.redb", name)), name).unwrap()
            }
        }
    }

    fn db(&self) -> &Database {
        self.db.as_ref().unwrap()
    }

    /// Close and reopen the database; data must survive (no-op for memory).
    fn reopen(&mut self) {
        if self.kind == Kind::Redb {
            self.db = None; // release the file lock first
            self.db = Some(Database::open(self.dir.path().join("db.redb"), "conformance").unwrap());
        }
    }
}

macro_rules! conformance {
    ($group:ident: $($name:ident),+ $(,)?) => {
        mod $group {
            mod memory {
                $(
                    #[tokio::test]
                    async fn $name() {
                        crate::$name(crate::Fx::new(crate::Kind::Memory)).await;
                    }
                )+
            }
            mod redb {
                $(
                    #[tokio::test]
                    async fn $name() {
                        crate::$name(crate::Fx::new(crate::Kind::Redb)).await;
                    }
                )+
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn doc(json: serde_json::Value) -> Document {
    Document::from_json(json).unwrap()
}

/// Write one document with `new_edits=true` and return its new rev.
async fn write(db: &Database, json: serde_json::Value) -> String {
    let res = db
        .bulk_docs(vec![doc(json)], BulkDocsOptions::new())
        .await
        .unwrap();
    assert!(res[0].ok, "write failed: {:?}", res[0]);
    res[0].rev.clone().unwrap()
}

/// Write one document with `new_edits=false` (replication mode).
async fn write_replicated(db: &Database, json: serde_json::Value) -> DocResult {
    let mut res = db
        .bulk_docs(vec![doc(json)], BulkDocsOptions::replication())
        .await
        .unwrap();
    res.remove(0)
}

async fn get_rev(db: &Database, id: &str, rev: &str) -> Result<Document> {
    db.get_with_opts(
        id,
        GetOptions {
            rev: Some(rev.to_string()),
            ..Default::default()
        },
    )
    .await
}

fn generation(rev: &str) -> u64 {
    rev.split('-').next().unwrap().parse().unwrap()
}

fn hash32(c: char) -> String {
    std::iter::repeat_n(c, 32).collect()
}

/// Build `doc` with two live leaves (2-aaa… and 2-zzz…, 2-zzz… wins).
async fn make_conflict(db: &Database, id: &str) -> (String, String, String) {
    let r1 = write(db, serde_json::json!({"_id": id, "v": 1})).await;
    let h1 = r1.split('-').nth(1).unwrap().to_string();
    let (a, z) = (hash32('a'), hash32('z'));
    for (h, v) in [(&a, "a"), (&z, "z")] {
        let r = write_replicated(
            db,
            serde_json::json!({
                "_id": id, "_rev": format!("2-{}", h), "v": v,
                "_revisions": {"start": 2, "ids": [h, h1]}
            }),
        )
        .await;
        assert!(r.ok, "{:?}", r);
    }
    (r1, format!("2-{}", a), format!("2-{}", z))
}

// === section: basics ===

async fn crud_roundtrip(fx: Fx) {
    let db = fx.db();
    let r1 = db.put("a", serde_json::json!({"v": 1})).await.unwrap();
    assert!(r1.ok);
    let got = db.get("a").await.unwrap();
    assert_eq!(got.data["v"], 1);
    let r2 = db
        .update("a", r1.rev.as_ref().unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();
    assert!(r2.ok);
    assert_eq!(db.get("a").await.unwrap().data["v"], 2);
    let r3 = db.remove("a", r2.rev.as_ref().unwrap()).await.unwrap();
    assert!(r3.ok);
    assert!(matches!(db.get("a").await, Err(RouchError::NotFound(_))));
    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 0);
    assert_eq!(info.doc_del_count, 1);
    assert_eq!(info.update_seq, Seq::Num(3));
}

async fn all_docs_ranges(fx: Fx) {
    let db = fx.db();
    for id in ["a", "b", "c", "d", "e"] {
        write(db, serde_json::json!({"_id": id, "n": id})).await;
    }
    let ids = |r: &AllDocsResponse| r.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(ids(&all), ["a", "b", "c", "d", "e"]);
    assert_eq!(all.total_rows, 5);
    let range = db
        .all_docs(AllDocsOptions {
            start_key: Some("b".into()),
            end_key: Some("d".into()),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(&range), ["b", "c", "d"]);
    let desc = db
        .all_docs(AllDocsOptions {
            descending: true,
            skip: 1,
            limit: Some(2),
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(&desc), ["d", "c"]);
    assert_eq!(desc.rows[0].doc.as_ref().unwrap()["n"], "d");
    let exclusive = db
        .all_docs(AllDocsOptions {
            end_key: Some("c".into()),
            inclusive_end: false,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(&exclusive), ["a", "b"]);
}

async fn changes_one_entry_per_doc(fx: Fx) {
    let db = fx.db();
    let r = write(db, serde_json::json!({"_id": "a", "v": 1})).await;
    write(db, serde_json::json!({"_id": "b"})).await;
    write(db, serde_json::json!({"_id": "a", "_rev": r, "v": 2})).await;
    let ch = db.changes(ChangesOptions::default()).await.unwrap();
    let ids: Vec<_> = ch.results.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["b", "a"]);
    assert_eq!(ch.last_seq, Seq::Num(3));
    let since = db
        .changes(ChangesOptions {
            since: Seq::Num(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(since.results.len(), 1);
}

conformance!(basics: crud_roundtrip, all_docs_ranges, changes_one_entry_per_doc);

// === section: f01 ===

/// F01: a long linear history must stay readable (and writable) on disk.
async fn long_history_survives_reopen(mut fx: Fx) {
    let mut rev = write(fx.db(), serde_json::json!({"_id": "d", "v": 0})).await;
    for i in 1..200 {
        rev = write(
            fx.db(),
            serde_json::json!({"_id": "d", "_rev": rev, "v": i}),
        )
        .await;
    }
    assert_eq!(generation(&rev), 200);
    fx.reopen();
    let db = fx.db();
    let got = db.get("d").await.unwrap();
    assert_eq!(got.rev.unwrap().to_string(), rev);
    assert_eq!(got.data["v"], 199);
    assert_eq!(db.info().await.unwrap().doc_count, 1);
    assert_eq!(
        db.all_docs(AllDocsOptions::new()).await.unwrap().rows.len(),
        1
    );
    // Still writable with the current rev, and no duplicate history.
    let next = write(db, serde_json::json!({"_id": "d", "_rev": rev, "v": 200})).await;
    assert_eq!(generation(&next), 201);
    let ch = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(ch.results.len(), 1);
}

/// F01: a replicated document with a 1000-revision ancestry.
async fn replicated_long_ancestry(mut fx: Fx) {
    let ids: Vec<String> = (0..1000).map(|i| format!("{:032x}", 1000 - i)).collect();
    let res = write_replicated(
        fx.db(),
        serde_json::json!({
            "_id": "d", "_rev": format!("1000-{}", ids[0]), "v": 1,
            "_revisions": {"start": 1000, "ids": ids}
        }),
    )
    .await;
    assert!(res.ok, "{:?}", res);
    fx.reopen();
    let db = fx.db();
    let got = db.get("d").await.unwrap();
    assert_eq!(got.rev.unwrap().pos, 1000);
    assert_eq!(db.info().await.unwrap().doc_count, 1);
    let bulk = db
        .adapter()
        .bulk_get(vec![BulkGetItem {
            id: "d".into(),
            rev: None,
        }])
        .await
        .unwrap();
    let doc = bulk.results[0].docs[0].ok.as_ref().unwrap();
    assert_eq!(doc["_revisions"]["ids"].as_array().unwrap().len(), 1000);
}

conformance!(f01: long_history_survives_reopen, replicated_long_ancestry);

// === section: attachments ===

/// F02: a body-only update keeps the parent's attachments.
async fn update_keeps_attachments(mut fx: Fx) {
    let r1 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = fx
        .db()
        .put_attachment("d", "a.txt", &r1, b"hello".to_vec(), "text/plain")
        .await
        .unwrap();
    let r3 = write(
        fx.db(),
        serde_json::json!({"_id": "d", "_rev": r2.rev.unwrap(), "v": 2}),
    )
    .await;
    fx.reopen();
    let db = fx.db();
    assert_eq!(db.get_attachment("d", "a.txt").await.unwrap(), b"hello");
    let got = db.get("d").await.unwrap();
    assert_eq!(got.rev.unwrap().to_string(), r3);
    assert!(got.attachments["a.txt"].stub);
    assert_eq!(got.attachments["a.txt"].length, 5);
}

/// F02 + F03: the minimal CouchDB inline form through bulk_docs.
async fn inline_attachment_through_bulk_docs(fx: Fx) {
    let db = fx.db();
    write(
        db,
        serde_json::json!({
            "_id": "d",
            "_attachments": {"hi.txt": {"content_type": "text/plain", "data": "aGkh"}}
        }),
    )
    .await;
    assert_eq!(db.get_attachment("d", "hi.txt").await.unwrap(), b"hi!");
    let got = db.get("d").await.unwrap();
    assert_eq!(got.attachments["hi.txt"].length, 3);
    assert_eq!(got.attachments["hi.txt"].content_type, "text/plain");
}

/// F05: attachment bytes belong to a revision, not to (doc, name).
async fn attachment_bytes_per_revision(mut fx: Fx) {
    let r1 = write(fx.db(), serde_json::json!({"_id": "d"})).await;
    let r2 = fx
        .db()
        .put_attachment("d", "a", &r1, b"v1".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let r3 = fx
        .db()
        .put_attachment("d", "a", &r2, b"v2".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let r4 = fx
        .db()
        .remove_attachment("d", "a", &r3)
        .await
        .unwrap()
        .rev
        .unwrap();
    fx.reopen();
    let db = fx.db();
    let at = |rev: &str| GetAttachmentOptions {
        rev: Some(rev.to_string()),
    };
    assert_eq!(
        db.get_attachment_with_opts("d", "a", at(&r2))
            .await
            .unwrap(),
        b"v1"
    );
    assert_eq!(
        db.get_attachment_with_opts("d", "a", at(&r3))
            .await
            .unwrap(),
        b"v2"
    );
    assert!(
        db.get_attachment_with_opts("d", "a", at(&r4))
            .await
            .is_err()
    );
    assert!(db.get_attachment("d", "a").await.is_err());
}

/// F05: two conflicting branches keep their own bytes for the same name.
async fn conflict_branches_keep_own_attachment(fx: Fx) {
    let db = fx.db();
    let (_r1, loser, winner) = make_conflict(db, "d").await;
    let wl = db
        .put_attachment("d", "a", &loser, b"loser".to_vec(), "text/plain")
        .await
        .unwrap();
    assert!(wl.ok, "{:?}", wl);
    let ww = db
        .put_attachment("d", "a", &winner, b"winner".to_vec(), "text/plain")
        .await
        .unwrap();
    let at = |rev: &str| GetAttachmentOptions {
        rev: Some(rev.to_string()),
    };
    assert_eq!(
        db.get_attachment_with_opts("d", "a", at(wl.rev.as_ref().unwrap()))
            .await
            .unwrap(),
        b"loser"
    );
    assert_eq!(
        db.get_attachment_with_opts("d", "a", at(ww.rev.as_ref().unwrap()))
            .await
            .unwrap(),
        b"winner"
    );
}

/// F02: bulk_get (the replication source path) carries attachment bytes.
async fn bulk_get_carries_attachments(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d"})).await;
    db.put_attachment(
        "d",
        "a.bin",
        &r1,
        vec![0, 1, 2, 255],
        "application/octet-stream",
    )
    .await
    .unwrap();
    let res = db
        .adapter()
        .bulk_get(vec![BulkGetItem {
            id: "d".into(),
            rev: None,
        }])
        .await
        .unwrap();
    let json = res.results[0].docs[0].ok.clone().unwrap();
    let parsed = Document::from_json(json).unwrap();
    assert_eq!(
        parsed.attachments["a.bin"].data.as_deref(),
        Some(&[0, 1, 2, 255][..])
    );
}

/// F02 (replication): attachments survive replication to and from each
/// adapter.
async fn replication_carries_attachments(fx: Fx) {
    let src = fx.db();
    let r1 = write(src, serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = src
        .put_attachment("d", "a.txt", &r1, b"payload".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    write(src, serde_json::json!({"_id": "d", "_rev": r2, "v": 2})).await;

    for target in [Database::memory("t"), fx.sibling("t")] {
        let res = src.replicate_to(&target).await.unwrap();
        assert!(res.ok, "{:?}", res);
        assert_eq!(
            target.get_attachment("d", "a.txt").await.unwrap(),
            b"payload"
        );
        // And back into a fresh database of the adapter under test.
        let back = fx.sibling(&format!("back{}", uuid::Uuid::new_v4().simple()));
        target.replicate_to(&back).await.unwrap();
        assert_eq!(back.get_attachment("d", "a.txt").await.unwrap(), b"payload");
        assert_eq!(back.get("d").await.unwrap().data["v"], 2);
    }
}

conformance!(attachments:
    update_keeps_attachments,
    inline_attachment_through_bulk_docs,
    attachment_bytes_per_revision,
    conflict_branches_keep_own_attachment,
    bulk_get_carries_attachments,
    replication_carries_attachments,
);

// === section: f06 ===

/// F06: attachment-only edits from the same parent on two replicas must
/// produce different revisions, so replication surfaces the conflict.
async fn attachment_only_edits_diverge(fx: Fx) {
    let a = fx.db();
    let b = fx.sibling("b");
    let r1 = write(a, serde_json::json!({"_id": "d"})).await;
    a.replicate_to(&b).await.unwrap();
    let ra = a
        .put_attachment("d", "f", &r1, b"AAA".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let rb = b
        .put_attachment("d", "f", &r1, b"BBB".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    assert_ne!(ra, rb);
    a.sync(&b).await.unwrap();
    for db in [a, &b] {
        let got = db
            .get_with_opts(
                "d",
                GetOptions {
                    conflicts: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(got.data["_conflicts"].as_array().unwrap().len(), 1);
    }
}

conformance!(f06: attachment_only_edits_diverge);

// === section: conflicts ===

/// F08: a losing conflict leaf can be deleted, which resolves the conflict.
async fn remove_losing_leaf(fx: Fx) {
    let db = fx.db();
    let (_r1, loser, winner) = make_conflict(db, "d").await;
    let mut tomb = doc(serde_json::json!({"_id": "d", "_rev": loser, "_deleted": true}));
    tomb.data = serde_json::json!({});
    let res = db
        .bulk_docs(vec![tomb], BulkDocsOptions::new())
        .await
        .unwrap();
    assert!(res[0].ok, "{:?}", res[0]);
    let got = db
        .get_with_opts(
            "d",
            GetOptions {
                conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(got.rev.unwrap().to_string(), winner);
    assert!(got.data.get("_conflicts").is_none(), "{}", got.data);
}

/// F08: a losing conflict leaf can be updated.
async fn update_losing_leaf(fx: Fx) {
    let db = fx.db();
    let (_r1, loser, _winner) = make_conflict(db, "d").await;
    let rev = write(
        db,
        serde_json::json!({"_id": "d", "_rev": loser, "v": "fixed"}),
    )
    .await;
    assert_eq!(generation(&rev), 3);
    // The extended branch is now the winner (higher generation).
    assert_eq!(db.get("d").await.unwrap().data["v"], "fixed");
}

/// F08: editing a non-leaf revision is still a conflict.
async fn update_non_leaf_conflicts(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d", "v": 1})).await;
    write(db, serde_json::json!({"_id": "d", "_rev": r1, "v": 2})).await;
    let res = db
        .bulk_docs(
            vec![doc(serde_json::json!({"_id": "d", "_rev": r1, "v": 3}))],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
    assert_eq!(res[0].error.as_deref(), Some("conflict"));
}

/// F09: deleting the winner of a conflict leaves the other branch live, so
/// the changes feed must not report the document as deleted.
async fn changes_deleted_follows_winner(fx: Fx) {
    let db = fx.db();
    let (_r1, _loser, winner) = make_conflict(db, "d").await;
    let res = db
        .bulk_docs(
            vec![doc(
                serde_json::json!({"_id": "d", "_rev": winner, "_deleted": true}),
            )],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
    assert!(res[0].ok);
    assert_eq!(db.get("d").await.unwrap().data["v"], "a");
    let ch = db
        .changes(ChangesOptions {
            include_docs: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let ev = ch.results.iter().find(|c| c.id == "d").unwrap();
    assert!(!ev.deleted);
    let d = ev.doc.as_ref().unwrap();
    assert!(d.get("_deleted").is_none(), "{}", d);
    assert_eq!(d["v"], "a");
    assert_eq!(db.info().await.unwrap().doc_count, 1);
}

/// Re-creating a deleted document (the PR #7 case) extends its tombstone
/// and does not resurrect the old attachments.
async fn recreate_after_delete(mut fx: Fx) {
    let r1 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = fx
        .db()
        .put_attachment("d", "a.txt", &r1, b"old".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    fx.db().remove("d", &r2).await.unwrap();
    // Same body as the very first revision.
    let r4 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    assert_eq!(generation(&r4), 4);
    fx.reopen();
    let db = fx.db();
    let got = db.get("d").await.unwrap();
    assert_eq!(got.rev.unwrap().to_string(), r4);
    assert!(got.attachments.is_empty());
    assert!(db.get_attachment("d", "a.txt").await.is_err());
    let r5 = db
        .put_attachment("d", "b.txt", &r4, b"new".to_vec(), "text/plain")
        .await
        .unwrap();
    assert!(r5.ok);
    assert_eq!(db.get_attachment("d", "b.txt").await.unwrap(), b"new");
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (1, 0));
}

conformance!(conflicts:
    remove_losing_leaf,
    update_losing_leaf,
    update_non_leaf_conflicts,
    changes_deleted_follows_winner,
    recreate_after_delete,
);

// === section: get ===

/// F10: an unknown revision is `not_found`, never an empty document.
async fn get_unknown_rev_not_found(fx: Fx) {
    let db = fx.db();
    write(db, serde_json::json!({"_id": "d", "v": 1})).await;
    assert!(matches!(
        get_rev(db, "d", "9-deadbeef").await,
        Err(RouchError::NotFound(_))
    ));
    assert!(matches!(
        get_rev(db, "d", "not-a-rev").await,
        Err(RouchError::InvalidRev(_))
    ));
}

/// F28: `latest` follows the requested revision's branch to its leaf.
async fn get_latest(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = write(db, serde_json::json!({"_id": "d", "_rev": r1, "v": 2})).await;
    let got = db
        .get_with_opts(
            "d",
            GetOptions {
                rev: Some(r1),
                latest: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(got.rev.unwrap().to_string(), r2);
    assert_eq!(got.data["v"], 2);
}

/// F28: `revs` and `revs_info`.
async fn get_revs_and_revs_info(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = write(db, serde_json::json!({"_id": "d", "_rev": r1, "v": 2})).await;
    let got = db
        .get_with_opts(
            "d",
            GetOptions {
                revs: true,
                revs_info: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let revisions = &got.data["_revisions"];
    assert_eq!(revisions["start"], 2);
    let h = |r: &str| r.split('-').nth(1).unwrap().to_string();
    assert_eq!(revisions["ids"], serde_json::json!([h(&r2), h(&r1)]));
    let info = got.data["_revs_info"].as_array().unwrap();
    assert_eq!(info.len(), 2);
    assert_eq!(info[0]["rev"], r2);
    assert_eq!(info[0]["status"], "available");
    assert_eq!(info[1]["rev"], r1);
}

/// F28 / F84: revs_info lists the requested revision's own ancestry.
async fn revs_info_follows_branch(fx: Fx) {
    let db = fx.db();
    let (r1, loser, winner) = make_conflict(db, "d").await;
    let got = db
        .get_with_opts(
            "d",
            GetOptions {
                revs_info: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let revs: Vec<&str> = got.data["_revs_info"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["rev"].as_str().unwrap())
        .collect();
    assert_eq!(revs, [winner.as_str(), r1.as_str()]);
    assert!(!revs.contains(&loser.as_str()));
}

/// F28: `attachments: true` inlines the bytes.
async fn get_inline_attachments(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d"})).await;
    db.put_attachment("d", "a", &r1, b"bytes".to_vec(), "text/plain")
        .await
        .unwrap();
    let got = db
        .get_with_opts(
            "d",
            GetOptions {
                attachments: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(got.attachments["a"].data.as_deref(), Some(&b"bytes"[..]));
    assert!(!got.attachments["a"].stub);
}

/// F28: `open_revs` cannot be expressed by `get`; it must be rejected rather
/// than silently returning the winner.
async fn get_open_revs_rejected(fx: Fx) {
    let db = fx.db();
    write(db, serde_json::json!({"_id": "d"})).await;
    let res = db
        .get_with_opts(
            "d",
            GetOptions {
                open_revs: Some(OpenRevs::All),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(res, Err(RouchError::BadRequest(_))), "{:?}", res);
}

conformance!(get:
    get_unknown_rev_not_found,
    get_latest,
    get_revs_and_revs_info,
    revs_info_follows_branch,
    get_inline_attachments,
    get_open_revs_rejected,
);

// === section: f22 ===

/// F22: re-sending an already stored revision in replication mode is a
/// no-op: no new sequence, no body or attachment overwrite.
async fn replicated_duplicate_is_noop(fx: Fx) {
    let db = fx.db();
    let h = hash32('b');
    let first = write_replicated(
        db,
        serde_json::json!({
            "_id": "d", "_rev": format!("1-{}", h), "v": 1,
            "_attachments": {"a": {"content_type": "text/plain", "data": "aGkh"}}
        }),
    )
    .await;
    assert!(first.ok);
    let seq = db.info().await.unwrap().update_seq;
    let again = write_replicated(
        db,
        serde_json::json!({"_id": "d", "_rev": format!("1-{}", h), "v": 99}),
    )
    .await;
    assert!(again.ok);
    assert_eq!(db.info().await.unwrap().update_seq, seq);
    assert_eq!(db.get("d").await.unwrap().data["v"], 1);
    assert_eq!(db.get_attachment("d", "a").await.unwrap(), b"hi!");
    assert_eq!(
        db.changes(ChangesOptions::default())
            .await
            .unwrap()
            .results
            .len(),
        1
    );
}

conformance!(f22: replicated_duplicate_is_noop);

// === section: f25 ===

/// F25: the security document is stored (and persisted).
async fn security_roundtrip(mut fx: Fx) {
    let sec = SecurityDocument {
        admins: SecurityGroup {
            names: vec!["bob".into()],
            roles: vec![],
        },
        members: SecurityGroup {
            names: vec![],
            roles: vec!["team".into()],
        },
        extra: Default::default(),
    };
    fx.db().put_security(sec).await.unwrap();
    fx.reopen();
    let got = fx.db().get_security().await.unwrap();
    assert_eq!(got.admins.names, ["bob"]);
    assert_eq!(got.members.roles, ["team"]);
}

conformance!(f25: security_roundtrip);

// === section: compact ===

/// F26 / F84: compaction drops non-leaf bodies and marks them missing.
async fn compact_drops_old_revisions(mut fx: Fx) {
    let r1 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = fx
        .db()
        .put_attachment("d", "a", &r1, b"old".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let r3 = fx
        .db()
        .put_attachment("d", "a", &r2, b"new".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    fx.db().compact().await.unwrap();
    fx.reopen();
    let db = fx.db();
    assert!(matches!(
        get_rev(db, "d", &r1).await,
        Err(RouchError::NotFound(_))
    ));
    assert!(matches!(
        get_rev(db, "d", &r2).await,
        Err(RouchError::NotFound(_))
    ));
    assert_eq!(get_rev(db, "d", &r3).await.unwrap().data["v"], 1);
    assert_eq!(db.get_attachment("d", "a").await.unwrap(), b"new");
    let got = db
        .get_with_opts(
            "d",
            GetOptions {
                revs_info: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let statuses: Vec<&str> = got.data["_revs_info"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["available", "missing", "missing"]);
    // Still writable after compaction.
    write(db, serde_json::json!({"_id": "d", "_rev": r3, "v": 2})).await;
    assert_eq!(db.get_attachment("d", "a").await.unwrap(), b"new");
}

conformance!(compact: compact_drops_old_revisions);

// === section: f27 ===

/// F27: a tombstone does not carry the attachments forward.
async fn delete_drops_attachments(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d"})).await;
    let r2 = db
        .put_attachment("d", "big", &r1, vec![7; 1024], "application/octet-stream")
        .await
        .unwrap()
        .rev
        .unwrap();
    let r3 = db.remove("d", &r2).await.unwrap().rev.unwrap();
    let tomb = get_rev(db, "d", &r3).await.unwrap();
    assert!(tomb.deleted);
    assert!(tomb.attachments.is_empty());
}

/// F27: an explicit `_attachments` object is the exact new set: stubs keep
/// an attachment, omitted ones are removed.
async fn explicit_attachment_set_is_exact(fx: Fx) {
    let db = fx.db();
    let r1 = write(
        db,
        serde_json::json!({
            "_id": "d",
            "_attachments": {
                "keep": {"content_type": "text/plain", "data": "a2VlcA=="},
                "drop": {"content_type": "text/plain", "data": "ZHJvcA=="}
            }
        }),
    )
    .await;
    // GET, then PUT back with only one stub (what Fauxton or a CouchDB
    // client does after removing an attachment in the editor).
    let mut json = get_rev(db, "d", &r1).await.unwrap().to_json();
    json["_attachments"].as_object_mut().unwrap().remove("drop");
    json["v"] = serde_json::json!(2);
    write(db, json).await;
    assert_eq!(db.get_attachment("d", "keep").await.unwrap(), b"keep");
    assert!(db.get_attachment("d", "drop").await.is_err());
}

conformance!(f27: delete_drops_attachments, explicit_attachment_set_is_exact);

// === section: f30 ===

/// F30: `keys` returns rows in request order (duplicates included), skips
/// unknown keys, reports deleted docs, and `descending` reverses the keys.
async fn all_docs_keys_order(fx: Fx) {
    let db = fx.db();
    for id in ["a", "b", "c"] {
        write(db, serde_json::json!({"_id": id})).await;
    }
    let r = write(db, serde_json::json!({"_id": "gone"})).await;
    db.remove("gone", &r).await.unwrap();
    let keys = |k: &[&str], descending: bool| AllDocsOptions {
        keys: Some(k.iter().map(|s| s.to_string()).collect()),
        descending,
        ..AllDocsOptions::new()
    };
    let ids = |r: &AllDocsResponse| r.rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>();
    let res = db
        .all_docs(keys(&["c", "a", "c", "zz", "gone"], false))
        .await
        .unwrap();
    assert_eq!(ids(&res), ["c", "a", "c", "gone"]);
    assert_eq!(res.rows[3].value.deleted, Some(true));
    let res = db.all_docs(keys(&["c", "a", "b"], true)).await.unwrap();
    assert_eq!(ids(&res), ["b", "a", "c"]);
    let res = db
        .all_docs(AllDocsOptions {
            key: Some("b".into()),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(&res), ["b"]);
}

conformance!(f30: all_docs_keys_order);

// === section: f68 ===

/// F68: document counts stay exact through every kind of write (they are
/// maintained incrementally by redb) and survive a reopen.
async fn info_counts_track_writes(mut fx: Fx) {
    let db = fx.db();
    let ra = write(db, serde_json::json!({"_id": "a"})).await;
    write(db, serde_json::json!({"_id": "b"})).await;
    let rc = write(db, serde_json::json!({"_id": "c"})).await;
    db.remove("a", &ra).await.unwrap();
    let rc2 = db
        .put_attachment("c", "x", &rc, b"x".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    db.remove("c", &rc2).await.unwrap();
    write(db, serde_json::json!({"_id": "c", "back": true})).await; // re-create
    make_conflict(db, "e").await;
    write_replicated(
        db,
        serde_json::json!({"_id": "f", "_rev": format!("1-{}", hash32('f')), "_deleted": true}),
    )
    .await;
    let check = |info: DbInfo| {
        assert_eq!((info.doc_count, info.doc_del_count), (3, 2), "{:?}", info);
    };
    check(fx.db().info().await.unwrap());
    let all = fx.db().all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(all.total_rows, 3);
    fx.reopen();
    check(fx.db().info().await.unwrap());
}

conformance!(f68: info_counts_track_writes);

// === section: f83_f86 ===

/// F83: a stub that references no stored attachment is rejected.
async fn unknown_stub_rejected(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d"})).await;
    let res = db
        .bulk_docs(
            vec![doc(serde_json::json!({
                "_id": "d", "_rev": r1,
                "_attachments": {"x": {"stub": true, "digest": "md5-bogus", "content_type": "text/plain"}}
            }))],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
    assert!(!res[0].ok);
    assert_eq!(res[0].error.as_deref(), Some("missing_stub"));
}

/// F86: removing an attachment that does not exist fails and writes nothing.
async fn remove_missing_attachment(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d"})).await;
    let seq = db.info().await.unwrap().update_seq;
    assert!(matches!(
        db.remove_attachment("d", "nope", &r1).await,
        Err(RouchError::NotFound(_))
    ));
    assert_eq!(db.info().await.unwrap().update_seq, seq);
    assert_eq!(db.get("d").await.unwrap().rev.unwrap().to_string(), r1);
}

conformance!(f83_f86: unknown_stub_rejected, remove_missing_attachment);

// === section: purge ===

/// F11: purging the only leaf removes the document; it never resurrects an
/// older revision.
async fn purge_leaf_removes_doc(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = write(db, serde_json::json!({"_id": "d", "_rev": r1, "v": 2})).await;
    let seq = db.info().await.unwrap().update_seq.as_num();
    let res = db.purge("d", vec![r2.clone()]).await.unwrap();
    assert_eq!(res.purged["d"], [r2]);
    assert!(matches!(db.get("d").await, Err(RouchError::NotFound(_))));
    assert!(matches!(
        get_rev(db, "d", &r1).await,
        Err(RouchError::NotFound(_))
    ));
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (0, 0));
    assert!(info.update_seq.as_num() > seq);
}

/// F11: purging a non-leaf revision is ignored; purging a conflict loser
/// keeps the winner and records a change.
async fn purge_conflict_loser(fx: Fx) {
    let db = fx.db();
    let (r1, loser, winner) = make_conflict(db, "d").await;
    let res = db.purge("d", vec![r1.clone()]).await.unwrap();
    assert!(res.purged.get("d").is_none_or(|v| v.is_empty()));
    let before = db.info().await.unwrap().update_seq.as_num();
    let res = db.purge("d", vec![loser.clone()]).await.unwrap();
    assert_eq!(res.purged["d"], vec![loser.clone()]);
    let got = db
        .get_with_opts(
            "d",
            GetOptions {
                conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(got.rev.unwrap().to_string(), winner);
    assert!(got.data.get("_conflicts").is_none());
    assert!(matches!(
        get_rev(db, "d", &loser).await,
        Err(RouchError::NotFound(_))
    ));
    let ch = db.changes(ChangesOptions::default()).await.unwrap();
    assert!(ch.results.last().unwrap().seq.as_num() > before);
}

conformance!(purge: purge_leaf_removes_doc, purge_conflict_loser);

// === section: facade ===

/// F07: reserved members in a `put` body are interpreted, not stored.
async fn put_interprets_reserved_members(fx: Fx) {
    let db = fx.db();
    db.put("gone", serde_json::json!({"_deleted": true, "x": 1}))
        .await
        .unwrap();
    assert!(matches!(db.get("gone").await, Err(RouchError::NotFound(_))));

    let r1 = db.put("d", serde_json::json!({"v": 1})).await.unwrap();
    let mut got = db
        .get_with_opts(
            "d",
            GetOptions {
                conflicts: true,
                revs_info: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    got.data["_conflicts"] = serde_json::json!(["2-x"]);
    let r2 = db
        .update("d", r1.rev.as_ref().unwrap(), got.data.clone())
        .await
        .unwrap();
    let stored = get_rev(db, "d", r2.rev.as_ref().unwrap()).await.unwrap();
    assert_eq!(stored.data, serde_json::json!({"v": 1}));

    assert!(matches!(
        db.put("bad", serde_json::json!({"_foo": 1})).await,
        Err(RouchError::BadRequest(_))
    ));
    assert!(matches!(db.get("bad").await, Err(RouchError::NotFound(_))));
}

/// F64: non-object bodies are rejected instead of being stored as `{}`.
async fn put_rejects_non_object(fx: Fx) {
    let db = fx.db();
    assert!(matches!(
        db.put("a", serde_json::json!([1, 2, 3])).await,
        Err(RouchError::BadRequest(_))
    ));
    assert!(db.post(serde_json::json!("text")).await.is_err());
    assert_eq!(db.info().await.unwrap().update_seq, Seq::Num(0));
}

/// F57: a failed single-document write is an error, not `Ok(ok:false)`.
async fn single_doc_failures_are_errors(fx: Fx) {
    let db = fx.db();
    let r1 = db.put("d", serde_json::json!({"v": 1})).await.unwrap();
    assert!(matches!(
        db.put("d", serde_json::json!({"v": 1})).await,
        Err(RouchError::Conflict)
    ));
    let r1 = r1.rev.unwrap();
    db.update("d", &r1, serde_json::json!({"v": 2}))
        .await
        .unwrap();
    assert!(matches!(
        db.update("d", &r1, serde_json::json!({"v": 3})).await,
        Err(RouchError::Conflict)
    ));
    assert!(matches!(
        db.remove("d", &r1).await,
        Err(RouchError::Conflict)
    ));
    assert!(matches!(
        db.update("missing", "1-abc", serde_json::json!({})).await,
        Err(RouchError::Conflict)
    ));
}

/// F57: `post` honours an `_id` in the body.
async fn post_uses_body_id(fx: Fx) {
    let db = fx.db();
    let res = db
        .post(serde_json::json!({"_id": "chosen", "v": 1}))
        .await
        .unwrap();
    assert_eq!(res.id, "chosen");
    assert_eq!(
        db.get("chosen").await.unwrap().data,
        serde_json::json!({"v": 1})
    );
}

conformance!(facade:
    put_interprets_reserved_members,
    put_rejects_non_object,
    single_doc_failures_are_errors,
    post_uses_body_id,
);

// === section: storage fidelity ===
//
// Each scenario pins the behaviour of CouchDB 3.5.1 (checked with curl) so
// memory and redb cannot drift from it (or from each other).

/// A database of the fixture's kind whose adapter stems histories to
/// `limit` revisions (`name` selects the redb file, so it can be reopened).
fn with_rev_limit(fx: &Fx, name: &str, limit: u64) -> Database {
    use std::sync::Arc;
    match fx.kind {
        Kind::Memory => {
            Database::from_adapter(Arc::new(MemoryAdapter::new(name).with_rev_limit(limit)))
        }
        Kind::Redb => Database::from_adapter(Arc::new(
            RedbAdapter::open(fx.dir.path().join(format!("{name}.redb")), name)
                .unwrap()
                .with_rev_limit(limit),
        )),
    }
}

/// A document body whose deepest path crosses `depth` containers (the
/// top-level object included): `{"v": [[...[1]...]]}`.
fn nested(depth: usize) -> serde_json::Value {
    let mut v = serde_json::json!(1);
    for _ in 1..depth {
        v = serde_json::Value::Array(vec![v]);
    }
    serde_json::json!({ "v": v })
}

/// Q-CORE-3: ids that share a prefix with another id up to a NUL (which
/// CouchDB accepts, so they arrive by replication) are separate documents:
/// compacting or purging one never touches the other's bodies.
async fn unusual_ids_survive_compact_and_purge(mut fx: Fx) {
    let ids = [
        "a",
        "a\u{0}b",
        "a\u{0}",
        "\u{1F600}",
        "ä/\\x",
        "x",
        "x\u{0}y",
    ];
    for (i, id) in ids.iter().enumerate() {
        let r1 = write(fx.db(), serde_json::json!({"_id": id, "n": i})).await;
        write(fx.db(), serde_json::json!({"_id": id, "_rev": r1, "n": i})).await;
    }
    fx.db().compact().await.unwrap();
    let x = fx.db().get("x").await.unwrap().rev.unwrap().to_string();
    let res = fx.db().purge("x", vec![x.clone()]).await.unwrap();
    assert_eq!(res.purged["x"], [x]);
    fx.reopen();
    let db = fx.db();
    for (i, id) in ids.iter().enumerate() {
        if *id == "x" {
            assert!(matches!(db.get(id).await, Err(RouchError::NotFound(_))));
            continue;
        }
        let got = db.get(id).await.unwrap_or_else(|e| panic!("{id:?}: {e}"));
        assert_eq!(got.data, serde_json::json!({"n": i}), "{id:?}");
        assert_eq!(got.rev.unwrap().pos, 2, "{id:?}");
    }
    let all = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(all.rows.len(), ids.len() - 1);
    assert!(all.rows.iter().all(|r| r.doc.is_some()));
    // Compacting again (after the purge) still keeps every body.
    db.compact().await.unwrap();
    assert_eq!(db.get("a\u{0}b").await.unwrap().data["n"], 1);
    assert_eq!(db.get("x\u{0}y").await.unwrap().data["n"], 6);
}

/// Q-API-3: CouchDB accepts (and serves) deeply nested documents; every
/// read path must decode what a write accepted, and the limit rouchdb
/// enforces is applied consistently at write time.
async fn deep_documents_round_trip(mut fx: Fx) {
    let deep = nested(300);
    fx.db().put("deep", deep.clone()).await.unwrap();
    let edge = nested(MAX_NESTING_DEPTH);
    fx.db().put("edge", edge.clone()).await.unwrap();
    assert!(matches!(
        fx.db().put("over", nested(MAX_NESTING_DEPTH + 1)).await,
        Err(RouchError::BadRequest(_))
    ));
    let mut over = nested(MAX_NESTING_DEPTH + 1);
    over["_id"] = "over".into();
    over["_rev"] = format!("1-{}", hash32('a')).into();
    let res = write_replicated(fx.db(), over).await;
    assert!(!res.ok);
    assert_eq!(res.error.as_deref(), Some("bad_request"));
    fx.reopen();

    let db = fx.db();
    assert_eq!(db.get("deep").await.unwrap().data, deep);
    assert_eq!(db.get("edge").await.unwrap().data, edge);
    assert!(matches!(db.get("over").await, Err(RouchError::NotFound(_))));
    let changes = db
        .changes(ChangesOptions {
            include_docs: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(changes.results.len(), 2);
    assert_eq!(changes.results[0].doc.as_ref().unwrap()["v"], deep["v"]);
    let all = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(all.rows[0].doc.as_ref().unwrap()["v"], deep["v"]);
    let found = db
        .find(FindOptions {
            selector: serde_json::json!({"v": {"$type": "array"}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.docs.len(), 2);
    let target = fx.sibling("deep_target");
    let rep = db.replicate_to(&target).await.unwrap();
    assert_eq!(rep.docs_written, 2);
    assert_eq!(target.get("deep").await.unwrap().data, deep);
    db.compact().await.unwrap();
    assert_eq!(db.get("edge").await.unwrap().data, edge);
}

/// Q-CORE-1: revisions stemmed by the revision limit are gone, like in
/// CouchDB: `get` and `bulk_get` report them missing and `revs_diff` asks
/// for them again; the bodies of the kept (non-leaf) revisions stay
/// readable until compaction.
async fn stemmed_revisions_are_unreadable(fx: Fx) {
    let mut db = with_rev_limit(&fx, "stem", 5);
    let mut revs = vec![write(&db, serde_json::json!({"_id": "d", "v": 1})).await];
    for v in 2..=8 {
        let prev = revs.last().unwrap().clone();
        revs.push(write(&db, serde_json::json!({"_id": "d", "_rev": prev, "v": v})).await);
    }
    for _ in 0..2 {
        for (i, rev) in revs.iter().enumerate() {
            let got = get_rev(&db, "d", rev).await;
            if i < 3 {
                assert!(
                    matches!(got, Err(RouchError::NotFound(_))),
                    "{rev}: {got:?}"
                );
            } else {
                assert_eq!(got.unwrap().data["v"], i + 1);
            }
        }
        let bulk = db
            .adapter()
            .bulk_get(vec![BulkGetItem {
                id: "d".into(),
                rev: Some(revs[0].clone()),
            }])
            .await
            .unwrap();
        assert!(bulk.results[0].docs[0].ok.is_none());
        let got = db
            .get_with_opts(
                "d",
                GetOptions {
                    revs: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(got.data["_revisions"]["ids"].as_array().unwrap().len(), 5);
        let diff = db
            .adapter()
            .revs_diff(std::collections::HashMap::from([(
                "d".to_string(),
                vec![revs[0].clone()],
            )]))
            .await
            .unwrap();
        assert_eq!(diff.results["d"].missing, [revs[0].clone()]);
        if fx.kind == Kind::Memory {
            break;
        }
        // Reopen: stemmed revisions stay gone.
        db.close().await.unwrap();
        drop(db);
        db = with_rev_limit(&fx, "stem", 5);
    }
}

/// Q-CORE-9: `_revisions` must be well formed; CouchDB rejects non-string
/// revision ids instead of dropping them (which shifted the ancestry and
/// invented revisions).
async fn replicated_revisions_are_validated(fx: Fx) {
    let db = fx.db();
    let (a, c) = (hash32('a'), hash32('c'));
    let bad = [
        (
            serde_json::json!({"start": 3, "ids": [c, 5, a]}),
            "RevId isn't a string",
        ),
        (
            serde_json::json!({"start": 3, "ids": [c, null]}),
            "RevId isn't a string",
        ),
        (
            serde_json::json!({"start": "3", "ids": [c]}),
            "_revisions.start isn't an integer.",
        ),
        (
            serde_json::json!({"ids": [c]}),
            "_revisions.start isn't an integer.",
        ),
        (
            serde_json::json!({"start": 3, "ids": c}),
            "_revisions.ids isn't a array.",
        ),
        (
            serde_json::json!([1]),
            "Bad special document member: _revisions",
        ),
    ];
    for (revisions, reason) in bad {
        let res = write_replicated(
            db,
            serde_json::json!({"_id": "d", "_rev": format!("3-{c}"), "_revisions": revisions}),
        )
        .await;
        assert!(!res.ok, "{revisions}");
        assert_eq!(res.error.as_deref(), Some("doc_validation"), "{revisions}");
        assert_eq!(res.reason.as_deref(), Some(reason), "{revisions}");
    }
    assert!(matches!(db.get("d").await, Err(RouchError::NotFound(_))));
    assert_eq!(db.info().await.unwrap().update_seq, Seq::Num(0));
}

/// Q-CORE-10: `revs_diff` returns exactly what CouchDB does: missing revs
/// in (generation, id) order, and every leaf older than a missing rev as a
/// possible ancestor, once, in winner order (deleted leaves last).
async fn revs_diff_matches_couchdb(fx: Fx) {
    let db = fx.db();
    let (x, a, b, e, f) = (
        hash32('1'),
        hash32('a'),
        hash32('b'),
        hash32('e'),
        hash32('f'),
    );
    let rev = |pos: u64, h: &str| format!("{pos}-{h}");
    let diff = |revs: Vec<String>| async move {
        let res = db
            .adapter()
            .revs_diff(std::collections::HashMap::from([("rd".to_string(), revs)]))
            .await
            .unwrap();
        res.results
            .get("rd")
            .map(|r| (r.missing.clone(), r.possible_ancestors.clone()))
    };
    assert!(
        write_replicated(db, serde_json::json!({"_id": "rd", "_rev": rev(1, &x)}))
            .await
            .ok
    );
    assert_eq!(
        diff(vec![rev(2, &a), rev(3, &b), rev(1, &x)]).await,
        Some((vec![rev(2, &a), rev(3, &b)], vec![rev(1, &x)]))
    );
    for doc in [
        serde_json::json!({"_id": "rd", "_rev": rev(2, &a), "_revisions": {"start": 2, "ids": [a, x]}}),
        serde_json::json!({"_id": "rd", "_rev": rev(2, &b), "_deleted": true, "_revisions": {"start": 2, "ids": [b, x]}}),
        serde_json::json!({"_id": "rd", "_rev": rev(3, &e), "_revisions": {"start": 3, "ids": [e, e, x]}}),
    ] {
        assert!(write_replicated(db, doc).await.ok);
    }
    let all = vec![rev(3, &e), rev(2, &a), rev(2, &b)];
    assert_eq!(
        diff(vec![rev(3, &f), rev(4, &f), rev(2, &f)]).await,
        Some((vec![rev(2, &f), rev(3, &f), rev(4, &f)], all.clone()))
    );
    assert_eq!(
        diff(vec![rev(3, &f)]).await,
        Some((vec![rev(3, &f)], vec![rev(2, &a), rev(2, &b)]))
    );
    assert_eq!(
        diff(vec![rev(2, &f)]).await,
        Some((vec![rev(2, &f)], vec![]))
    );
    // Duplicates are kept (as CouchDB does); ancestors are listed once.
    assert_eq!(
        diff(vec![rev(4, &f), rev(4, &f)]).await,
        Some((vec![rev(4, &f), rev(4, &f)], all))
    );
    // Upper-case ids are the same revisions (Q-CORE-17).
    assert_eq!(diff(vec![rev(3, &e.to_uppercase())]).await, None);
    assert_eq!(
        diff(vec![rev(3, &f.to_uppercase())]).await,
        Some((vec![rev(3, &f)], vec![rev(2, &a), rev(2, &b)]))
    );
}

/// Q-CORE-11: stubs are matched by name (the stored metadata wins); a stub
/// under a name the parent does not have is `missing_stub`, even when its
/// digest matches another attachment.
async fn stubs_match_by_name(fx: Fx) {
    let db = fx.db();
    let r1 = write(
        db,
        serde_json::json!({"_id": "d", "_attachments": {"a.txt": {"content_type": "text/plain", "data": "SGVsbG8="}}}),
    )
    .await;
    let stored = get_rev(db, "d", &r1).await.unwrap().attachments["a.txt"].clone();
    let r2 = write(
        db,
        serde_json::json!({"_id": "d", "_rev": r1, "v": 1, "_attachments": {"a.txt": {
            "stub": true, "digest": "md5-AAAAAAAAAAAAAAAAAAAAAA==", "content_type": "image/png", "length": 999
        }}}),
    )
    .await;
    let r3 = write(
        db,
        serde_json::json!({"_id": "d", "_rev": r2, "v": 2, "_attachments": {"a.txt": {"stub": true}}}),
    )
    .await;
    let got = get_rev(db, "d", &r3).await.unwrap();
    let att = &got.attachments["a.txt"];
    assert_eq!(
        (&att.digest, &att.content_type, att.length),
        (&stored.digest, &stored.content_type, stored.length)
    );
    assert_eq!(db.get_attachment("d", "a.txt").await.unwrap(), b"Hello");
    let res = db
        .bulk_docs(
            vec![doc(serde_json::json!({
                "_id": "d", "_rev": r3, "_attachments": {"b.txt": {"stub": true, "digest": stored.digest}}
            }))],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
    assert_eq!(res[0].error.as_deref(), Some("missing_stub"));
    assert_eq!(
        res[0].reason.as_deref(),
        Some("Invalid attachment stub in d for b.txt")
    );
}

/// Q-CORE-8: an edit that names a revision of a document that does not
/// exist is a conflict (CouchDB PUT and `_bulk_docs`, PouchDB); `remove`
/// of such a document is `not_found` like CouchDB's DELETE; a malformed
/// revision is `InvalidRev` (CouchDB: 400 "Invalid rev format").
async fn edits_of_missing_documents(fx: Fx) {
    let db = fx.db();
    let rev = format!("1-{}", hash32('a'));
    for deleted in [false, true] {
        let res = db
            .bulk_docs(
                vec![doc(
                    serde_json::json!({"_id": "nodoc", "_rev": rev, "_deleted": deleted}),
                )],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap();
        assert_eq!(res[0].error.as_deref(), Some("conflict"));
    }
    assert!(matches!(
        db.put("nodoc", serde_json::json!({"_rev": rev})).await,
        Err(RouchError::Conflict)
    ));
    assert!(matches!(
        db.update("nodoc", &rev, serde_json::json!({})).await,
        Err(RouchError::Conflict)
    ));
    assert!(matches!(
        db.remove("nodoc", &rev).await,
        Err(RouchError::NotFound(_))
    ));
    let r1 = write(db, serde_json::json!({"_id": "d"})).await;
    let r2 = db.remove("d", &r1).await.unwrap().rev.unwrap();
    assert!(matches!(
        db.remove("d", &r2).await,
        Err(RouchError::NotFound(_))
    ));
    assert_eq!(db.info().await.unwrap().update_seq, Seq::Num(2));
    for bad in ["not-a-rev", "abc", "x-1"] {
        let got = get_rev(db, "d", bad).await;
        assert!(
            matches!(got, Err(RouchError::InvalidRev(_))),
            "{bad}: {got:?}"
        );
    }
}

/// Q-API-4: `_local/` ids are local documents (like CouchDB and the http
/// adapter): not listed, not in the changes feed, never replicated, and
/// versioned `0-N` without MVCC.
async fn local_ids_are_local_documents(mut fx: Fx) {
    let db = fx.db();
    let r = db
        .put("_local/x", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    assert_eq!((r.id.as_str(), r.rev.as_deref()), ("_local/x", Some("0-1")));
    let r = db
        .update("_local/x", "0-1", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    assert_eq!(r.rev.as_deref(), Some("0-2"));
    let res = db
        .bulk_docs(
            vec![
                doc(serde_json::json!({"_id": "_local/y", "v": 3, "_rev": "0-7"})),
                doc(serde_json::json!({"_id": "_local/z", "_rev": "1-abc"})),
            ],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
    assert_eq!(res[0].rev.as_deref(), Some("0-8"));
    assert!(!res[1].ok);
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.update_seq), (0, Seq::Num(0)));
    assert!(
        db.all_docs(AllDocsOptions::new())
            .await
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(
        db.changes(ChangesOptions::default())
            .await
            .unwrap()
            .results
            .is_empty()
    );
    fx.reopen();
    let db = fx.db();
    let got = db.get("_local/x").await.unwrap();
    assert_eq!(got.id, "_local/x");
    assert_eq!(got.rev.unwrap().to_string(), "0-2");
    assert_eq!(got.data, serde_json::json!({"v": 2}));
    assert_eq!(db.adapter().get_local("x").await.unwrap()["v"], 2);
    let target = fx.sibling("local_target");
    db.put("real", serde_json::json!({})).await.unwrap();
    db.replicate_to(&target).await.unwrap();
    assert!(matches!(
        target.get("_local/x").await,
        Err(RouchError::NotFound(_))
    ));
    // `_local/_security` is an ordinary local document, not the security
    // document.
    let mut sec = SecurityDocument::default();
    sec.members.roles.push("team".into());
    db.put_security(sec).await.unwrap();
    assert!(matches!(
        db.get("_local/_security").await,
        Err(RouchError::NotFound(_))
    ));
    db.put(
        "_local/_security",
        serde_json::json!({"admins": {"names": ["eve"]}}),
    )
    .await
    .unwrap();
    let sec = db.get_security().await.unwrap();
    assert!(sec.admins.names.is_empty());
    assert_eq!(sec.members.roles, ["team"]);
    let r = db.remove("_local/x", "0-2").await.unwrap();
    assert_eq!(r.rev.as_deref(), Some("0-0"));
    assert!(matches!(
        db.get("_local/x").await,
        Err(RouchError::NotFound(_))
    ));
    assert!(matches!(
        db.remove("_local/x", "0-2").await,
        Err(RouchError::NotFound(_))
    ));
}

/// Q-API-7: a failed `put_design` is an error, like `put`.
async fn put_design_conflict_is_an_error(fx: Fx) {
    let db = fx.db();
    let ddoc = || DesignDocument {
        id: "_design/app".into(),
        rev: None,
        views: std::collections::HashMap::new(),
        filters: std::collections::HashMap::new(),
        validate_doc_update: None,
        shows: std::collections::HashMap::new(),
        lists: std::collections::HashMap::new(),
        updates: std::collections::HashMap::new(),
        language: None,
    };
    assert!(db.put_design(ddoc()).await.unwrap().ok);
    assert!(matches!(
        db.put_design(ddoc()).await,
        Err(RouchError::Conflict)
    ));
}

/// Q-API-11: `destroy` drops everything (documents, local documents such
/// as replication checkpoints, attachments, security) and the handle then
/// behaves as a new, empty database.
async fn destroy_resets_the_database(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d"})).await;
    db.put_attachment("d", "a", &r1, b"x".to_vec(), "text/plain")
        .await
        .unwrap();
    db.adapter()
        .put_local("checkpoint", serde_json::json!({"last_seq": 2}))
        .await
        .unwrap();
    db.put("_local/other", serde_json::json!({})).await.unwrap();
    let mut sec = SecurityDocument::default();
    sec.admins.names.push("bob".into());
    db.put_security(sec).await.unwrap();
    db.destroy().await.unwrap();
    let info = db.info().await.unwrap();
    assert_eq!(
        (info.doc_count, info.doc_del_count, info.update_seq),
        (0, 0, Seq::Num(0))
    );
    assert!(matches!(
        db.adapter().get_local("checkpoint").await,
        Err(RouchError::NotFound(_))
    ));
    assert!(matches!(
        db.get("_local/other").await,
        Err(RouchError::NotFound(_))
    ));
    assert!(db.get_security().await.unwrap().admins.names.is_empty());
    assert!(db.get_attachment("d", "a").await.is_err());
    let r = db.put("d", serde_json::json!({"v": 2})).await.unwrap();
    assert_eq!(generation(r.rev.as_deref().unwrap()), 1);
    assert_eq!(db.info().await.unwrap().update_seq, Seq::Num(1));
}

/// Q-CORE-17: CouchDB stores 32-digit hex revision ids in lower case, so
/// an upper-case id is the same revision (and sorts like it for the
/// winner).
async fn uppercase_revisions_are_normalized(mut fx: Fx) {
    let (up_f, a) = (hash32('F'), hash32('a'));
    let base = hash32('0');
    for (h, v) in [(&up_f, "f"), (&a, "a")] {
        let res = write_replicated(
            fx.db(),
            serde_json::json!({
                "_id": "d", "_rev": format!("2-{h}"), "v": v,
                "_revisions": {"start": 2, "ids": [h, base.to_uppercase()]}
            }),
        )
        .await;
        assert!(res.ok, "{res:?}");
    }
    fx.reopen();
    let db = fx.db();
    let got = db
        .get_with_opts(
            "d",
            GetOptions {
                revs: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(got.rev.unwrap().to_string(), format!("2-{}", hash32('f')));
    assert_eq!(
        got.data["_revisions"],
        serde_json::json!({"start": 2, "ids": [hash32('f'), base]})
    );
    let old = get_rev(db, "d", &format!("2-{up_f}")).await.unwrap();
    assert_eq!(old.data["v"], "f");
    let next = db
        .update("d", &format!("2-{up_f}"), serde_json::json!({"v": 3}))
        .await
        .unwrap();
    assert_eq!(generation(next.rev.as_deref().unwrap()), 3);
}

/// A `changes` request with `limit: 0` returns no change (CouchDB 3.5.1:
/// empty `results`; `last_seq` is `since`, or the current sequence when
/// descending).
async fn changes_limit_zero_is_empty(fx: Fx) {
    let db = fx.db();
    for id in ["a", "b", "c"] {
        write(db, serde_json::json!({"_id": id})).await;
    }
    for (descending, since, last) in [(false, 0, 0), (false, 1, 1), (true, 0, 3)] {
        let res = db
            .changes(ChangesOptions {
                limit: Some(0),
                descending,
                since: Seq::Num(since),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(res.results.is_empty(), "{descending} {since}: {res:?}");
        assert_eq!(res.last_seq, Seq::Num(last), "{descending} {since}");
    }
    let one = db
        .changes(ChangesOptions {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(one.results.len(), 1);
}

/// Re-sending an old edit of a deleted document (same parent, same body,
/// so the same revision id) is a conflict, as in CouchDB: it must not
/// report success or rewrite the stored revision.
async fn old_edit_of_deleted_document_conflicts(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = write(db, serde_json::json!({"_id": "d", "_rev": r1, "v": 2})).await;
    db.remove("d", &r2).await.unwrap();
    let seq = db.info().await.unwrap().update_seq;
    for v in [2, 9] {
        assert!(matches!(
            db.update("d", &r1, serde_json::json!({"v": v})).await,
            Err(RouchError::Conflict)
        ));
    }
    assert_eq!(db.info().await.unwrap().update_seq, seq);
    assert_eq!(get_rev(db, "d", &r2).await.unwrap().data["v"], 2);
    // Re-creating it (no rev) extends the tombstone.
    let r4 = db.put("d", serde_json::json!({"v": 3})).await.unwrap();
    assert_eq!(generation(r4.rev.as_deref().unwrap()), 4);
}

conformance!(storage_fidelity:
    unusual_ids_survive_compact_and_purge,
    deep_documents_round_trip,
    stemmed_revisions_are_unreadable,
    replicated_revisions_are_validated,
    revs_diff_matches_couchdb,
    stubs_match_by_name,
    edits_of_missing_documents,
    local_ids_are_local_documents,
    put_design_conflict_is_an_error,
    destroy_resets_the_database,
    uppercase_revisions_are_normalized,
    changes_limit_zero_is_empty,
    old_edit_of_deleted_document_conflicts,
);
