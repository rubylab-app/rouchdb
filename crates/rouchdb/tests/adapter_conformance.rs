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

/// The hash part of a `pos-hash` revision.
fn hash_of(rev: &str) -> String {
    rev.split_once('-').unwrap().1.to_string()
}

fn row_ids(r: &AllDocsResponse) -> Vec<String> {
    r.rows.iter().map(|r| r.key.clone()).collect()
}

/// The change events as JSON, so a scenario can compare them exactly
/// (seq, id, revisions, deleted flag, doc and conflicts). Like CouchDB,
/// `deleted` only appears when it is true.
fn changes_json(ch: &ChangesResponse) -> serde_json::Value {
    serde_json::to_value(&ch.results).unwrap()
}

async fn get_with_conflicts(db: &Database, id: &str) -> Document {
    db.get_with_opts(
        id,
        GetOptions {
            conflicts: true,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

/// `(rev, status)` pairs of `revs_info` for `rev` (the winner when `None`).
async fn revs_info(db: &Database, id: &str, rev: Option<&str>) -> Vec<(String, String)> {
    let got = db
        .get_with_opts(
            id,
            GetOptions {
                rev: rev.map(String::from),
                revs_info: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    got.data["_revs_info"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["rev"].as_str().unwrap().to_string(),
                i["status"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Build `id` with two live leaves under its first revision: 2-aaa…
/// (`v: "a"`) and 2-fff… (`v: "f"`). Same generation, so the higher hash
/// 2-fff… wins; the fixture checks that before returning
/// `(first rev, loser 2-aaa…, winner 2-fff…)`.
async fn make_conflict(db: &Database, id: &str) -> (String, String, String) {
    let r1 = write(db, serde_json::json!({"_id": id, "v": 1})).await;
    let h1 = hash_of(&r1);
    let (a, f) = (hash32('a'), hash32('f'));
    for (h, v) in [(&a, "a"), (&f, "f")] {
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
    let (loser, winner) = (format!("2-{}", a), format!("2-{}", f));
    let got = get_with_conflicts(db, id).await;
    assert_eq!(
        got.rev.unwrap().to_string(),
        winner,
        "fixture: 2-fff… must win"
    );
    assert_eq!(
        got.data,
        serde_json::json!({"v": "f", "_conflicts": [loser]}),
        "fixture: winner body and conflicts"
    );
    assert_eq!(
        get_rev(db, id, &loser).await.unwrap().data,
        serde_json::json!({"v": "a"}),
        "fixture: loser body"
    );
    (r1, loser, winner)
}

// === section: basics ===

async fn crud_roundtrip(fx: Fx) {
    let db = fx.db();
    let r1 = db.put("a", serde_json::json!({"v": 1})).await.unwrap();
    assert!(r1.ok);
    assert_eq!(r1.id, "a");
    let r1 = r1.rev.unwrap();
    assert_eq!(generation(&r1), 1);
    let got = db.get("a").await.unwrap();
    assert_eq!(got.id, "a");
    assert_eq!(got.rev.unwrap().to_string(), r1);
    assert_eq!(got.data, serde_json::json!({"v": 1}));
    assert!(!got.deleted);
    let r2 = db
        .update("a", &r1, serde_json::json!({"v": 2}))
        .await
        .unwrap()
        .rev
        .unwrap();
    assert_eq!(generation(&r2), 2);
    let got = db.get("a").await.unwrap();
    assert_eq!(got.rev.unwrap().to_string(), r2);
    assert_eq!(got.data, serde_json::json!({"v": 2}));
    // The previous revision is still readable by rev.
    assert_eq!(
        get_rev(db, "a", &r1).await.unwrap().data,
        serde_json::json!({"v": 1})
    );
    let r3 = db.remove("a", &r2).await.unwrap().rev.unwrap();
    assert_eq!(generation(&r3), 3);
    assert!(matches!(db.get("a").await, Err(RouchError::NotFound(_))));
    // The tombstone itself is readable and carries no body.
    let tomb = get_rev(db, "a", &r3).await.unwrap();
    assert!(tomb.deleted);
    assert_eq!(tomb.data, serde_json::json!({}));
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
    let ids = |r: &AllDocsResponse| r.rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>();
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
    // Without include_docs no row carries a document.
    assert!(all.rows.iter().all(|r| r.doc.is_none()));
    // Descending swaps the bounds; inclusive_end applies to the end key in
    // both directions.
    let page = |descending: bool, start: Option<&str>, end: Option<&str>, inclusive_end: bool| {
        AllDocsOptions {
            descending,
            start_key: start.map(String::from),
            end_key: end.map(String::from),
            inclusive_end,
            ..AllDocsOptions::new()
        }
    };
    for (opts, expected) in [
        (page(true, Some("d"), Some("b"), true), vec!["d", "c", "b"]),
        (page(true, Some("d"), Some("b"), false), vec!["d", "c"]),
        (page(true, Some("c"), None, true), vec!["c", "b", "a"]),
        (page(true, None, Some("c"), true), vec!["e", "d", "c"]),
        (page(true, None, Some("c"), false), vec!["e", "d"]),
        (page(false, Some("b"), Some("d"), false), vec!["b", "c"]),
        (page(false, Some("bb"), Some("dd"), true), vec!["c", "d"]),
    ] {
        let desc = format!("{:?}", opts);
        let res = db.all_docs(opts).await.unwrap();
        assert_eq!(row_ids(&res), expected, "{}", desc);
        assert_eq!(res.total_rows, 5, "{}", desc);
    }
    // limit=0 returns no rows, whatever the selection.
    for opts in [
        AllDocsOptions {
            limit: Some(0),
            ..AllDocsOptions::new()
        },
        AllDocsOptions {
            key: Some("b".into()),
            limit: Some(0),
            ..AllDocsOptions::new()
        },
        AllDocsOptions {
            start_key: Some("b".into()),
            limit: Some(0),
            ..AllDocsOptions::new()
        },
    ] {
        let desc = format!("{:?}", opts);
        let res = db.all_docs(opts).await.unwrap();
        assert!(res.rows.is_empty(), "{}", desc);
        assert_eq!(res.total_rows, 5);
    }
    // skip + limit page through the range.
    let skipped = db
        .all_docs(AllDocsOptions {
            start_key: Some("b".into()),
            skip: 1,
            limit: Some(2),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(row_ids(&skipped), ["c", "d"]);
    // include_docs: the document, and `_conflicts` only when there are some.
    make_conflict(db, "k").await;
    let docs = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            conflicts: true,
            start_key: Some("d".into()),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(row_ids(&docs), ["d", "e", "k"]);
    let rev = |i: usize| docs.rows[i].rev().unwrap().to_string();
    assert_eq!(
        docs.rows[0].doc,
        Some(serde_json::json!({"_id": "d", "_rev": rev(0), "n": "d"}))
    );
    assert_eq!(
        docs.rows[2].doc,
        Some(serde_json::json!({
            "_id": "k", "_rev": rev(2), "v": "f",
            "_conflicts": [format!("2-{}", hash32('a'))]
        }))
    );
    let no_conflicts = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            key: Some("k".into()),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(
        no_conflicts.rows[0].doc,
        Some(serde_json::json!({"_id": "k", "_rev": rev(2), "v": "f"}))
    );
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
    assert_eq!(
        since
            .results
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
    assert_eq!(since.results[0].seq, Seq::Num(3));
    assert_eq!(since.last_seq, Seq::Num(3));
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
    let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(row_ids(&all), ["d"]);
    assert_eq!(all.rows[0].rev().unwrap(), rev);
    // Still writable with the current rev, and no duplicate history.
    let next = write(db, serde_json::json!({"_id": "d", "_rev": rev, "v": 200})).await;
    assert_eq!(generation(&next), 201);
    let ch = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(
        changes_json(&ch),
        serde_json::json!([{"seq": 201, "id": "d", "changes": [{"rev": next}]}])
    );
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
    assert_eq!(
        doc["_revisions"],
        serde_json::json!({"start": 1000, "ids": ids})
    );
}

/// Q-CORE-1: past the default rev_limit (1000) the stored history is
/// stemmed on write, so the ancestry reported for the winner never grows
/// beyond 1000 revisions. (A replicated 1000-revision history plus five
/// local edits, so the test does not need 1005 separate commits.)
async fn rev_limit_stems_long_histories(mut fx: Fx) {
    let ids: Vec<String> = (0..1000).map(|i| format!("{:032x}", 1000 - i)).collect();
    let res = write_replicated(
        fx.db(),
        serde_json::json!({
            "_id": "d", "_rev": format!("1000-{}", ids[0]), "v": 0,
            "_revisions": {"start": 1000, "ids": ids}
        }),
    )
    .await;
    assert!(res.ok, "{:?}", res);
    let mut rev = format!("1000-{}", ids[0]);
    for i in 1..=5 {
        rev = write(
            fx.db(),
            serde_json::json!({"_id": "d", "_rev": rev, "v": i}),
        )
        .await;
    }
    assert_eq!(generation(&rev), 1005);
    fx.reopen();
    let got = fx
        .db()
        .get_with_opts(
            "d",
            GetOptions {
                revs: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(got.rev.unwrap().to_string(), rev);
    assert_eq!(got.data["v"], 5);
    assert_eq!(got.data["_revisions"]["start"], 1005);
    let kept = got.data["_revisions"]["ids"].as_array().unwrap();
    assert_eq!(kept.len(), 1000);
    assert_eq!(kept[0], hash_of(&rev));
    // The oldest kept revision is the 6th generation of the original chain.
    assert_eq!(kept[999], serde_json::json!(format!("{:032x}", 6)));
    let info = fx.db().info().await.unwrap();
    assert_eq!((info.doc_count, info.update_seq), (1, Seq::Num(6)));
}

conformance!(f01:
    long_history_survives_reopen,
    replicated_long_ancestry,
    rev_limit_stems_long_histories,
);

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
    assert!(matches!(
        db.get_attachment_with_opts("d", "a", at(&r4)).await,
        Err(RouchError::NotFound(_))
    ));
    assert!(matches!(
        db.get_attachment("d", "a").await,
        Err(RouchError::NotFound(_))
    ));
}

/// F05: two conflicting branches keep their own bytes for the same name,
/// and an attachment edit on a branch keeps THAT branch's body.
async fn conflict_branches_keep_own_attachment(mut fx: Fx) {
    let (_r1, loser, winner) = make_conflict(fx.db(), "d").await;
    let wl = fx
        .db()
        .put_attachment("d", "a", &loser, b"loser".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let ww = fx
        .db()
        .put_attachment("d", "a", &winner, b"winner".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    assert_eq!((generation(&wl), generation(&ww)), (3, 3));
    fx.reopen();
    let db = fx.db();
    let at = |rev: &str| GetAttachmentOptions {
        rev: Some(rev.to_string()),
    };
    assert_eq!(
        db.get_attachment_with_opts("d", "a", at(&wl))
            .await
            .unwrap(),
        b"loser"
    );
    assert_eq!(
        db.get_attachment_with_opts("d", "a", at(&ww))
            .await
            .unwrap(),
        b"winner"
    );
    // Each new revision extends its own branch with its own body.
    let on_loser = get_rev(db, "d", &wl).await.unwrap();
    assert_eq!(on_loser.data, serde_json::json!({"v": "a"}));
    assert_eq!(on_loser.attachments["a"].length, 5);
    assert_eq!(
        revs_info(db, "d", Some(&wl))
            .await
            .into_iter()
            .map(|(r, _)| r)
            .collect::<Vec<_>>()[1],
        loser
    );
    let on_winner = get_rev(db, "d", &ww).await.unwrap();
    assert_eq!(on_winner.data, serde_json::json!({"v": "f"}));
    assert_eq!(on_winner.attachments["a"].length, 6);
    // Both branches are still in conflict; the higher hash wins.
    let (top, other) = if hash_of(&wl) > hash_of(&ww) {
        (&wl, &ww)
    } else {
        (&ww, &wl)
    };
    let got = get_with_conflicts(db, "d").await;
    assert_eq!(got.rev.unwrap().to_string(), *top);
    assert_eq!(got.data["_conflicts"], serde_json::json!([other]));
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
    let (top, other) = if hash_of(&ra) > hash_of(&rb) {
        (&ra, &rb)
    } else {
        (&rb, &ra)
    };
    for db in [a, &b] {
        let got = get_with_conflicts(db, "d").await;
        assert_eq!(got.rev.unwrap().to_string(), *top);
        assert_eq!(got.data["_conflicts"], serde_json::json!([other]));
    }
}

conformance!(f06: attachment_only_edits_diverge);

// === section: conflicts ===

/// F08: a losing conflict leaf can be deleted, which resolves the conflict.
async fn remove_losing_leaf(mut fx: Fx) {
    let (_r1, loser, winner) = make_conflict(fx.db(), "d").await;
    let mut tomb = doc(serde_json::json!({"_id": "d", "_rev": loser, "_deleted": true}));
    tomb.data = serde_json::json!({});
    let res = fx
        .db()
        .bulk_docs(vec![tomb], BulkDocsOptions::new())
        .await
        .unwrap();
    assert!(res[0].ok, "{:?}", res[0]);
    let tomb_rev = res[0].rev.clone().unwrap();
    assert_eq!(generation(&tomb_rev), 3);
    fx.reopen();
    let db = fx.db();
    let got = get_with_conflicts(db, "d").await;
    assert_eq!(got.rev.unwrap().to_string(), winner);
    assert_eq!(got.data, serde_json::json!({"v": "f"}));
    assert!(get_rev(db, "d", &tomb_rev).await.unwrap().deleted);
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (1, 0));
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
    // The extended branch is now the winner (higher generation) and the old
    // winner becomes the conflict.
    let got = get_with_conflicts(db, "d").await;
    assert_eq!(got.rev.unwrap().to_string(), rev);
    assert_eq!(
        got.data,
        serde_json::json!({"v": "fixed", "_conflicts": [_winner]})
    );
}

/// F08: editing a non-leaf revision is still a conflict.
async fn update_non_leaf_conflicts(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = write(db, serde_json::json!({"_id": "d", "_rev": r1, "v": 2})).await;
    let seq = db.info().await.unwrap().update_seq;
    // Also a retry of the very same edit (same parent, same body), and a
    // revision the document never had.
    let unknown = format!("1-{}", hash32('9'));
    for (rev, v) in [(&r1, 3), (&r1, 2), (&unknown, 3)] {
        let res = db
            .bulk_docs(
                vec![doc(serde_json::json!({"_id": "d", "_rev": rev, "v": v}))],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap();
        assert!(!res[0].ok);
        assert_eq!(
            res[0].error.as_deref(),
            Some("conflict"),
            "rev={} v={}",
            rev,
            v
        );
    }
    assert_eq!(db.info().await.unwrap().update_seq, seq);
    assert_eq!(db.get("d").await.unwrap().rev.unwrap().to_string(), r2);
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
    assert_eq!(got.data, serde_json::json!({"v": 1}));
    assert!(matches!(
        db.get_attachment("d", "a.txt").await,
        Err(RouchError::NotFound(_))
    ));
    let r5 = db
        .put_attachment("d", "b.txt", &r4, b"new".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    assert_eq!(generation(&r5), 5);
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
    assert_eq!(
        got.data["_revs_info"],
        serde_json::json!([
            {"rev": r2, "status": "available"},
            {"rev": r1, "status": "available"}
        ])
    );
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
    let ch = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(
        changes_json(&ch),
        serde_json::json!([{"seq": 1, "id": "d", "changes": [{"rev": format!("1-{}", h)}]}])
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
    // Compacting an empty database is a no-op.
    fx.db().compact().await.unwrap();
    assert_eq!(fx.db().info().await.unwrap().update_seq, Seq::Num(0));
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
    assert!(matches!(
        db.get_attachment("d", "drop").await,
        Err(RouchError::NotFound(_))
    ));
    let got = db.get("d").await.unwrap();
    assert_eq!(got.attachments.keys().collect::<Vec<_>>(), ["keep"]);
    assert_eq!(got.data, serde_json::json!({"v": 2}));
}

conformance!(f27: delete_drops_attachments, explicit_attachment_set_is_exact);

// === section: f30 ===

/// F30: `keys` returns one row per requested key, in request order
/// (duplicates included, reversed by `descending`), like CouchDB: a
/// document, a deleted document (`value.deleted`, never a `doc`) or a
/// `not_found` error row, and `skip`/`limit` count every kind of row.
async fn all_docs_keys_order(fx: Fx) {
    let db = fx.db();
    let mut revs = std::collections::HashMap::new();
    for id in ["a", "b", "c"] {
        revs.insert(id, write(db, serde_json::json!({"_id": id})).await);
    }
    let r = write(db, serde_json::json!({"_id": "gone"})).await;
    let gone = db.remove("gone", &r).await.unwrap().rev.unwrap();
    let keys = |k: &[&str], descending: bool| AllDocsOptions {
        keys: Some(k.iter().map(|s| s.to_string()).collect()),
        descending,
        ..AllDocsOptions::new()
    };
    let ids = |r: &AllDocsResponse| r.rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>();
    let res = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..keys(&["c", "a", "c", "zz", "gone"], false)
        })
        .await
        .unwrap();
    assert_eq!(ids(&res), ["c", "a", "c", "zz", "gone"]);
    let row = |id: &str, rev: &str, deleted: bool, doc: Option<serde_json::Value>| {
        AllDocsRow::document(id, AllDocsRowValue::new(rev, deleted)).with_doc(doc)
    };
    let body = |id: &str| Some(serde_json::json!({"_id": id, "_rev": revs[id]}));
    assert_eq!(res.rows[0], row("c", &revs["c"], false, body("c")));
    assert_eq!(res.rows[1], row("a", &revs["a"], false, body("a")));
    assert_eq!(res.rows[2], res.rows[0]);
    assert_eq!(res.rows[3], AllDocsRow::not_found("zz"));
    assert_eq!(res.rows[3].error.as_deref(), Some("not_found"));
    assert_eq!(res.rows[4], row("gone", &gone, true, None));
    assert!(res.rows[4].is_deleted() && !res.rows[4].is_error());

    let res = db.all_docs(keys(&["c", "a", "b"], true)).await.unwrap();
    assert_eq!(ids(&res), ["b", "a", "c"]);
    // Reversed to [a, zz, gone, a]; skip and limit count the error row.
    let res = db
        .all_docs(AllDocsOptions {
            skip: 1,
            limit: Some(2),
            ..keys(&["a", "gone", "zz", "a"], true)
        })
        .await
        .unwrap();
    assert_eq!(ids(&res), ["zz", "gone"]);
    assert!(res.rows[0].is_error() && res.rows[1].is_deleted());
    // `key` (not `keys`) never yields an error or deleted row.
    for key in ["zz", "gone"] {
        let res = db
            .all_docs(AllDocsOptions {
                key: Some(key.into()),
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        assert!(res.rows.is_empty(), "{key}: {:?}", res.rows);
    }
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

/// Accepted difference (book: "Differences from CouchDB"): the local
/// adapters report the `skip` as `offset`, like PouchDB's, where CouchDB
/// reports the global position of the first row (see
/// `offset_is_the_global_position_on_couchdb` in `all_docs.rs`).
async fn accepted_divergence_all_docs_offset_is_the_skip(fx: Fx) {
    let db = fx.db();
    for id in ["a", "b", "c", "d", "e"] {
        write(db, serde_json::json!({"_id": id})).await;
    }
    let from_c = db
        .all_docs(AllDocsOptions {
            start_key: Some("c".into()),
            skip: 1,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(row_ids(&from_c), ["d", "e"]);
    assert_eq!((from_c.total_rows, from_c.offset), (5, 1));
    let keys = db
        .all_docs(AllDocsOptions {
            keys: Some(vec!["a".into(), "b".into()]),
            skip: 1,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!((row_ids(&keys), keys.offset), (vec!["b".to_string()], 1));
}

conformance!(accepted_divergence: accepted_divergence_all_docs_offset_is_the_skip);

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
    assert_eq!(row_ids(&all), ["b", "c", "e"]);
    fx.reopen();
    check(fx.db().info().await.unwrap());
    let all = fx.db().all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(row_ids(&all), ["b", "c", "e"]);
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
/// older revision, and the purge survives a reopen.
async fn purge_leaf_removes_doc(mut fx: Fx) {
    let r1 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = write(fx.db(), serde_json::json!({"_id": "d", "_rev": r1, "v": 2})).await;
    write(fx.db(), serde_json::json!({"_id": "other"})).await;
    let seq = fx.db().info().await.unwrap().update_seq.as_num();
    let res = fx.db().purge("d", vec![r2.clone()]).await.unwrap();
    assert_eq!(res.purged["d"], std::slice::from_ref(&r2));
    assert_eq!(res.purged.len(), 1);
    fx.reopen();
    let db = fx.db();
    assert!(matches!(db.get("d").await, Err(RouchError::NotFound(_))));
    assert!(matches!(
        get_rev(db, "d", &r1).await,
        Err(RouchError::NotFound(_))
    ));
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (1, 0));
    // A purge is one database update.
    assert_eq!(info.update_seq.as_num(), seq + 1);
    assert_eq!(
        row_ids(&db.all_docs(AllDocsOptions::new()).await.unwrap()),
        ["other"]
    );
    let ch = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(
        ch.results.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["other"]
    );
    // Purging again finds nothing, and purge requests are counted.
    let again = db.purge("d", vec![r2.clone()]).await.unwrap();
    assert!(again.purged.get("d").is_none_or(|v| v.is_empty()));
    assert!(
        again.purge_seq > res.purge_seq,
        "{:?} then {:?}",
        res,
        again
    );
}

/// F11: purging a non-leaf revision is ignored; purging a conflict loser
/// keeps the winner and records a change.
async fn purge_conflict_loser(mut fx: Fx) {
    let (r1, loser, winner) = make_conflict(fx.db(), "d").await;
    let res = fx.db().purge("d", vec![r1.clone()]).await.unwrap();
    assert_eq!(res.purged["d"], Vec::<String>::new());
    let before = fx.db().info().await.unwrap().update_seq.as_num();
    let res = fx.db().purge("d", vec![loser.clone()]).await.unwrap();
    assert_eq!(res.purged["d"], vec![loser.clone()]);
    fx.reopen();
    let db = fx.db();
    let got = get_with_conflicts(db, "d").await;
    assert_eq!(got.rev.unwrap().to_string(), winner);
    assert_eq!(got.data, serde_json::json!({"v": "f"}));
    assert!(matches!(
        get_rev(db, "d", &loser).await,
        Err(RouchError::NotFound(_))
    ));
    // The purge re-records the document as one new change.
    let ch = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(
        changes_json(&ch),
        serde_json::json!([{"seq": before + 1, "id": "d", "changes": [{"rev": winner}]}])
    );
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
    assert!(matches!(
        db.post(serde_json::json!("text")).await,
        Err(RouchError::BadRequest(_))
    ));
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

// === section: conflicts_more ===

/// Compaction keeps the body of EVERY leaf (not only the winner) and drops
/// the shared ancestor's body; conflicts survive a reopen.
async fn compact_keeps_conflict_leaves(mut fx: Fx) {
    let (r1, loser, winner) = make_conflict(fx.db(), "d").await;
    fx.db().compact().await.unwrap();
    fx.reopen();
    let db = fx.db();
    assert_eq!(
        get_rev(db, "d", &loser).await.unwrap().data,
        serde_json::json!({"v": "a"})
    );
    assert_eq!(
        get_rev(db, "d", &winner).await.unwrap().data,
        serde_json::json!({"v": "f"})
    );
    assert!(matches!(
        get_rev(db, "d", &r1).await,
        Err(RouchError::NotFound(_))
    ));
    let got = get_with_conflicts(db, "d").await;
    assert_eq!(got.rev.unwrap().to_string(), winner);
    assert_eq!(got.data["_conflicts"], serde_json::json!([loser]));
    assert_eq!(
        revs_info(db, "d", Some(&loser)).await,
        [
            (loser.clone(), "available".to_string()),
            (r1.clone(), "missing".to_string())
        ]
    );
    // The losing branch is still editable after compaction.
    let r3 = write(
        db,
        serde_json::json!({"_id": "d", "_rev": loser, "v": "a2"}),
    )
    .await;
    assert_eq!(generation(&r3), 3);
    assert_eq!(db.get("d").await.unwrap().rev.unwrap().to_string(), r3);
}

/// A tombstone that becomes an internal node (the document was re-created
/// on top of it) is still reported as `deleted` by revs_info, as CouchDB
/// does, including after a reopen.
async fn recreate_revs_info_marks_tombstone_deleted(mut fx: Fx) {
    let r1 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = fx.db().remove("d", &r1).await.unwrap().rev.unwrap();
    let r3 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    assert_eq!(generation(&r3), 3);
    fx.reopen();
    let db = fx.db();
    assert_eq!(
        revs_info(db, "d", None).await,
        [
            (r3.clone(), "available".to_string()),
            (r2.clone(), "deleted".to_string()),
            (r1.clone(), "available".to_string())
        ]
    );
    let tomb = get_rev(db, "d", &r2).await.unwrap();
    assert!(tomb.deleted);
    assert_eq!(tomb.data, serde_json::json!({}));
}

/// A live leaf wins over a deleted leaf even when the tombstone is at a
/// higher generation (CouchDB ranks `deleted` before the position).
async fn live_leaf_beats_deeper_tombstone(mut fx: Fx) {
    let (x, y, a, b) = (hash32('0'), hash32('c'), hash32('a'), hash32('d'));
    let live = format!("2-{}", y);
    let tomb = format!("3-{}", b);
    let r = write_replicated(
        fx.db(),
        serde_json::json!({
            "_id": "d", "_rev": live, "v": "live",
            "_revisions": {"start": 2, "ids": [y, x]}
        }),
    )
    .await;
    assert!(r.ok, "{:?}", r);
    let r = write_replicated(
        fx.db(),
        serde_json::json!({
            "_id": "d", "_rev": tomb, "_deleted": true,
            "_revisions": {"start": 3, "ids": [b, a, x]}
        }),
    )
    .await;
    assert!(r.ok, "{:?}", r);
    fx.reopen();
    let db = fx.db();
    let got = get_with_conflicts(db, "d").await;
    assert_eq!(got.rev.unwrap().to_string(), live);
    // A deleted leaf is never listed as a conflict.
    assert_eq!(got.data, serde_json::json!({"v": "live"}));
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (1, 0));
    let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(row_ids(&all), ["d"]);
    assert_eq!(all.rows[0].rev().unwrap(), live);
    assert_eq!(all.rows[0].value.as_ref().unwrap().deleted, None);
    let ch = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(
        changes_json(&ch),
        serde_json::json!([{"seq": 2, "id": "d", "changes": [{"rev": live}]}])
    );
    let all_leaves = db
        .changes(ChangesOptions {
            style: ChangesStyle::AllDocs,
            ..Default::default()
        })
        .await
        .unwrap();
    let mut leaves: Vec<&str> = all_leaves.results[0]
        .changes
        .iter()
        .map(|c| c.rev.as_str())
        .collect();
    leaves.sort();
    assert_eq!(leaves, [live.as_str(), tomb.as_str()]);
}

conformance!(conflicts_more:
    compact_keeps_conflict_leaves,
    recreate_revs_info_marks_tombstone_deleted,
    live_leaf_beats_deeper_tombstone,
);

// === section: lifecycle ===

/// `destroy` removes documents, attachments, local documents, the changes
/// feed and the security document, and the database is usable afterwards.
async fn destroy_clears_everything(mut fx: Fx) {
    let r1 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    fx.db()
        .put_attachment("d", "a", &r1, b"bytes".to_vec(), "text/plain")
        .await
        .unwrap();
    make_conflict(fx.db(), "c").await;
    fx.db()
        .adapter()
        .put_local("ck", serde_json::json!({"seq": 7}))
        .await
        .unwrap();
    fx.db()
        .put_security(SecurityDocument {
            admins: SecurityGroup {
                names: vec!["bob".into()],
                roles: vec![],
            },
            ..Default::default()
        })
        .await
        .unwrap();

    fx.db().destroy().await.unwrap();
    for pass in ["after destroy", "after reopen"] {
        let db = fx.db();
        let info = db.info().await.unwrap();
        assert_eq!(
            (info.doc_count, info.doc_del_count, info.update_seq.clone()),
            (0, 0, Seq::Num(0)),
            "{}",
            pass
        );
        assert!(matches!(db.get("d").await, Err(RouchError::NotFound(_))));
        assert!(matches!(
            db.get_attachment("d", "a").await,
            Err(RouchError::NotFound(_))
        ));
        assert!(matches!(
            db.adapter().get_local("ck").await,
            Err(RouchError::NotFound(_))
        ));
        let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert!(all.rows.is_empty(), "{}", pass);
        assert_eq!(all.total_rows, 0);
        let ch = db.changes(ChangesOptions::default()).await.unwrap();
        assert!(ch.results.is_empty(), "{}", pass);
        assert_eq!(ch.last_seq, Seq::Num(0));
        let sec = db.get_security().await.unwrap();
        assert!(sec.admins.names.is_empty(), "{}: {:?}", pass, sec);
        fx.reopen();
    }
    // Writes start from scratch: a new first revision and sequence 1.
    let r = write(fx.db(), serde_json::json!({"_id": "d", "v": 2})).await;
    assert_eq!(generation(&r), 1);
    assert_eq!(fx.db().info().await.unwrap().update_seq, Seq::Num(1));
}

/// Local documents are stored apart: they are not documents, not in the
/// changes feed, do not bump update_seq, and persist.
async fn local_docs_are_not_documents(mut fx: Fx) {
    fn local(fx: &Fx) -> &dyn Adapter {
        fx.db().adapter()
    }
    local(&fx)
        .put_local("ck", serde_json::json!({"seq": 1}))
        .await
        .unwrap();
    local(&fx)
        .put_local("ck", serde_json::json!({"seq": 2, "history": [1]}))
        .await
        .unwrap();
    local(&fx)
        .put_local("other", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        local(&fx).get_local("ck").await.unwrap(),
        serde_json::json!({"seq": 2, "history": [1]})
    );
    let info = fx.db().info().await.unwrap();
    assert_eq!((info.doc_count, info.update_seq), (0, Seq::Num(0)));
    assert!(
        fx.db()
            .all_docs(AllDocsOptions::new())
            .await
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(
        fx.db()
            .changes(ChangesOptions::default())
            .await
            .unwrap()
            .results
            .is_empty()
    );
    assert!(matches!(
        fx.db().get("ck").await,
        Err(RouchError::NotFound(_))
    ));
    assert!(matches!(
        local(&fx).remove_local("missing").await,
        Err(RouchError::NotFound(_))
    ));
    assert!(matches!(
        local(&fx).get_local("missing").await,
        Err(RouchError::NotFound(_))
    ));

    fx.reopen();
    assert_eq!(
        local(&fx).get_local("ck").await.unwrap(),
        serde_json::json!({"seq": 2, "history": [1]})
    );
    local(&fx).remove_local("ck").await.unwrap();
    assert!(matches!(
        local(&fx).get_local("ck").await,
        Err(RouchError::NotFound(_))
    ));
    // Removing one local document leaves the others alone.
    assert_eq!(
        local(&fx).get_local("other").await.unwrap(),
        serde_json::json!({})
    );
    fx.reopen();
    assert!(matches!(
        local(&fx).get_local("ck").await,
        Err(RouchError::NotFound(_))
    ));
}

conformance!(lifecycle: destroy_clears_everything, local_docs_are_not_documents);

// === section: revs_diff ===

/// `revs_diff` (the replication negotiation) returns exactly the missing
/// revisions and, for existing documents, the leaves that could be their
/// ancestors (leaves of a lower generation, as CouchDB does). Documents with
/// nothing missing are omitted.
async fn revs_diff_reports_exact_missing(mut fx: Fx) {
    let d1 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    let d2 = write(fx.db(), serde_json::json!({"_id": "d", "_rev": d1, "v": 2})).await;
    let (_c1, loser, winner) = make_conflict(fx.db(), "c").await;
    fx.reopen();
    let db = fx.db();
    let (h3, h9) = (format!("3-{}", hash32('3')), format!("9-{}", hash32('9')));
    let req: std::collections::HashMap<String, Vec<String>> = [
        // Known revisions (leaf and ancestor): nothing missing.
        ("d".to_string(), vec![d1.clone(), d2.clone()]),
        // One newer revision: both current leaves are possible ancestors.
        ("c".to_string(), vec![loser.clone(), h3.clone()]),
        // A document that does not exist: everything missing, no ancestors.
        (
            "nope".to_string(),
            vec![format!("1-{}", hash32('1')), h9.clone()],
        ),
    ]
    .into();
    let diff = db.adapter().revs_diff(req).await.unwrap();
    let mut keys: Vec<&String> = diff.results.keys().collect();
    keys.sort();
    assert_eq!(keys, ["c", "nope"]);
    assert_eq!(diff.results["c"].missing, [h3]);
    let mut ancestors = diff.results["c"].possible_ancestors.clone();
    ancestors.sort();
    assert_eq!(ancestors, [loser.clone(), winner.clone()]);
    assert_eq!(
        diff.results["nope"].missing,
        [format!("1-{}", hash32('1')), h9]
    );
    assert!(diff.results["nope"].possible_ancestors.is_empty());

    // Only leaves of a LOWER generation are possible ancestors.
    for (missing, expected) in [
        (format!("2-{}", hash32('2')), vec![]),
        (format!("1-{}", hash32('1')), vec![]),
        (format!("4-{}", hash32('4')), vec![d2.clone()]),
    ] {
        let diff = db
            .adapter()
            .revs_diff([("d".to_string(), vec![missing.clone()])].into())
            .await
            .unwrap();
        assert_eq!(diff.results["d"].missing, std::slice::from_ref(&missing));
        assert_eq!(
            diff.results["d"].possible_ancestors, expected,
            "missing {}",
            missing
        );
    }
}

conformance!(revs_diff: revs_diff_reports_exact_missing);

// === section: changes_options ===

/// Every changes-feed option on the same history: one entry per document at
/// its latest sequence, in both directions, with limit, doc_ids, conflicts,
/// style=all_docs and include_docs.
async fn changes_options(mut fx: Fx) {
    let ra = write(fx.db(), serde_json::json!({"_id": "a", "v": 1})).await; // seq 1
    let rb = write(fx.db(), serde_json::json!({"_id": "b"})).await; // seq 2
    let rc = write(fx.db(), serde_json::json!({"_id": "c"})).await; // seq 3
    let ra2 = write(fx.db(), serde_json::json!({"_id": "a", "_rev": ra, "v": 2})).await; // 4
    let (_k1, loser, winner) = make_conflict(fx.db(), "k").await; // 5, 6, 7
    let rb2 = fx.db().remove("b", &rb).await.unwrap().rev.unwrap(); // seq 8
    fx.reopen();
    let db = fx.db();
    let feed = |opts: ChangesOptions| async move { db.changes(opts).await.unwrap() };
    let ids = |ch: &ChangesResponse| {
        ch.results
            .iter()
            .map(|c| (c.seq.as_num(), c.id.clone()))
            .collect::<Vec<_>>()
    };
    let s = |id: &str| id.to_string();

    let all = feed(ChangesOptions::default()).await;
    assert_eq!(
        changes_json(&all),
        serde_json::json!([
            {"seq": 3, "id": "c", "changes": [{"rev": rc}]},
            {"seq": 4, "id": "a", "changes": [{"rev": ra2}]},
            {"seq": 7, "id": "k", "changes": [{"rev": winner}]},
            {"seq": 8, "id": "b", "changes": [{"rev": rb2}], "deleted": true},
        ])
    );
    assert_eq!(all.last_seq, Seq::Num(8));

    let desc = feed(ChangesOptions {
        descending: true,
        ..Default::default()
    })
    .await;
    assert_eq!(
        ids(&desc),
        [(8, s("b")), (7, s("k")), (4, s("a")), (3, s("c"))]
    );

    let limited = feed(ChangesOptions {
        limit: Some(2),
        ..Default::default()
    })
    .await;
    assert_eq!(ids(&limited), [(3, s("c")), (4, s("a"))]);
    assert_eq!(limited.last_seq, Seq::Num(4));

    let desc_limited = feed(ChangesOptions {
        descending: true,
        limit: Some(2),
        ..Default::default()
    })
    .await;
    assert_eq!(ids(&desc_limited), [(8, s("b")), (7, s("k"))]);
    assert_eq!(desc_limited.last_seq, Seq::Num(7));

    let since = feed(ChangesOptions {
        since: Seq::Num(4),
        ..Default::default()
    })
    .await;
    assert_eq!(ids(&since), [(7, s("k")), (8, s("b"))]);
    let caught_up = feed(ChangesOptions {
        since: Seq::Num(8),
        ..Default::default()
    })
    .await;
    assert!(caught_up.results.is_empty());
    assert_eq!(caught_up.last_seq, Seq::Num(8));

    let by_id = feed(ChangesOptions {
        doc_ids: Some(vec![s("b"), s("a"), s("zz")]),
        ..Default::default()
    })
    .await;
    assert_eq!(ids(&by_id), [(4, s("a")), (8, s("b"))]);
    assert_eq!(by_id.last_seq, Seq::Num(8));

    let conflicts = feed(ChangesOptions {
        conflicts: true,
        ..Default::default()
    })
    .await;
    let with_conflicts: Vec<(String, Option<Vec<String>>)> = conflicts
        .results
        .iter()
        .map(|c| (c.id.clone(), c.conflicts.clone()))
        .collect();
    assert_eq!(
        with_conflicts,
        [
            (s("c"), None),
            (s("a"), None),
            (s("k"), Some(vec![loser.clone()])),
            (s("b"), None)
        ]
    );

    let leaves = feed(ChangesOptions {
        style: ChangesStyle::AllDocs,
        ..Default::default()
    })
    .await;
    let leaf_revs: Vec<(String, Vec<String>)> = leaves
        .results
        .iter()
        .map(|c| {
            let mut revs: Vec<String> = c.changes.iter().map(|r| r.rev.clone()).collect();
            revs.sort();
            (c.id.clone(), revs)
        })
        .collect();
    assert_eq!(
        leaf_revs,
        [
            (s("c"), vec![rc.clone()]),
            (s("a"), vec![ra2.clone()]),
            (s("k"), vec![loser.clone(), winner.clone()]),
            (s("b"), vec![rb2.clone()])
        ]
    );
    // The winner is listed first.
    assert_eq!(leaves.results[2].changes[0].rev, winner);

    let docs = feed(ChangesOptions {
        include_docs: true,
        doc_ids: Some(vec![s("k"), s("b")]),
        ..Default::default()
    })
    .await;
    assert_eq!(
        docs.results[0].doc,
        Some(serde_json::json!({"_id": "k", "_rev": winner, "v": "f"}))
    );
    assert_eq!(
        docs.results[1].doc,
        Some(serde_json::json!({"_id": "b", "_rev": rb2, "_deleted": true}))
    );
}

conformance!(changes: changes_options);

// === section: batches ===

/// A `new_edits=true` batch reports one result per document, in order, and
/// stores only the successful ones. A second write to the same `_id` in the
/// same batch conflicts, as in CouchDB.
async fn mixed_batch_results(mut fx: Fx) {
    let re = write(fx.db(), serde_json::json!({"_id": "e", "v": 1})).await;
    let seq = fx.db().info().await.unwrap().update_seq.as_num();
    let res = fx
        .db()
        .bulk_docs(
            vec![
                doc(serde_json::json!({"_id": "n1", "v": 1})),
                doc(serde_json::json!({"_id": "e", "v": 9})),
                doc(serde_json::json!({"_id": "n2"})),
                doc(serde_json::json!({"_id": "e", "_rev": re, "v": 2})),
                doc(serde_json::json!({"_id": "n1", "v": 2})),
                doc(serde_json::json!({"_id": "e", "_rev": re, "v": 3})),
            ],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
    let summary: Vec<(String, bool, Option<String>, Option<u64>)> = res
        .iter()
        .map(|r| {
            (
                r.id.clone(),
                r.ok,
                r.error.clone(),
                r.rev.as_deref().map(generation),
            )
        })
        .collect();
    let conflict = Some("conflict".to_string());
    assert_eq!(
        summary,
        [
            ("n1".to_string(), true, None, Some(1)),
            ("e".to_string(), false, conflict.clone(), None),
            ("n2".to_string(), true, None, Some(1)),
            ("e".to_string(), true, None, Some(2)),
            ("n1".to_string(), false, conflict.clone(), None),
            ("e".to_string(), false, conflict, None),
        ]
    );
    fx.reopen();
    let db = fx.db();
    // Exactly the three successful writes were applied.
    assert_eq!(db.info().await.unwrap().update_seq.as_num(), seq + 3);
    assert_eq!(
        db.get("n1").await.unwrap().data,
        serde_json::json!({"v": 1})
    );
    let e = db.get("e").await.unwrap();
    assert_eq!(e.rev.unwrap().to_string(), res[3].rev.clone().unwrap());
    assert_eq!(e.data, serde_json::json!({"v": 2}));
    assert!(
        get_with_conflicts(db, "e")
            .await
            .data
            .get("_conflicts")
            .is_none()
    );
    assert_eq!(
        row_ids(&db.all_docs(AllDocsOptions::new()).await.unwrap()),
        ["e", "n1", "n2"]
    );
}

conformance!(batches: mixed_batch_results);

// === section: ids ===

/// Non-ASCII ids (accents, CJK, emoji, spaces, slashes) round-trip through
/// every read path and sort by code point.
async fn unicode_ids_roundtrip(mut fx: Fx) {
    let ids = [
        "Zeta",
        "a b",
        "a/b",
        "a%2Fb",
        "café",
        "ñandú",
        "日本語",
        "🦀crab",
        "zz",
    ];
    for id in ids {
        write(fx.db(), serde_json::json!({"_id": id, "id": id})).await;
    }
    let r = fx
        .db()
        .get("🦀crab")
        .await
        .unwrap()
        .rev
        .unwrap()
        .to_string();
    fx.db()
        .put_attachment("🦀crab", "ñ.txt", &r, b"x".to_vec(), "text/plain")
        .await
        .unwrap();
    fx.reopen();
    let db = fx.db();
    for id in ids {
        let got = db.get(id).await.unwrap();
        assert_eq!(got.id, id);
        assert_eq!(got.data["id"], id);
    }
    assert_eq!(db.get_attachment("🦀crab", "ñ.txt").await.unwrap(), b"x");
    let mut sorted: Vec<&str> = ids.to_vec();
    sorted.sort(); // code point order == UTF-8 byte order
    let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(row_ids(&all), sorted);
    let range = db
        .all_docs(AllDocsOptions {
            start_key: Some("c".into()),
            end_key: Some("日".into()),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(row_ids(&range), ["café", "zz", "ñandú"]);
    let keyed = db
        .all_docs(AllDocsOptions {
            keys: Some(vec!["日本語".into(), "a/b".into()]),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(row_ids(&keyed), ["日本語", "a/b"]);
    let ch = db
        .changes(ChangesOptions {
            doc_ids: Some(vec!["ñandú".into()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ch.results.len(), 1);
    assert_eq!(ch.results[0].id, "ñandú");
    // And they replicate unchanged.
    let target = fx.sibling("unicode_target");
    db.replicate_to(&target).await.unwrap();
    let copied = target.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(row_ids(&copied), sorted);
    assert_eq!(
        target.get_attachment("🦀crab", "ñ.txt").await.unwrap(),
        b"x"
    );
}

conformance!(ids: unicode_ids_roundtrip);

// === section: writes ===

/// Documents without an `_id` get a generated UUID v4, distinct per doc.
async fn bulk_docs_generates_missing_ids(mut fx: Fx) {
    let res = fx
        .db()
        .bulk_docs(
            vec![
                doc(serde_json::json!({"v": 1})),
                doc(serde_json::json!({"v": 2})),
            ],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
    assert!(res.iter().all(|r| r.ok), "{:?}", res);
    assert_ne!(res[0].id, res[1].id);
    for r in &res {
        let id = uuid::Uuid::parse_str(&r.id).unwrap();
        assert_eq!(id.get_version_num(), 4, "{}", r.id);
        // 32 hex digits, like the ids CouchDB generates.
        assert_eq!(r.id, id.simple().to_string());
    }
    fx.reopen();
    for (r, v) in res.iter().zip([1, 2]) {
        let got = fx.db().get(&r.id).await.unwrap();
        assert_eq!(got.data, serde_json::json!({ "v": v }));
        assert_eq!(got.rev.map(|r| r.to_string()), r.rev);
    }
}

/// Attachment edits against a stale revision conflict and write nothing.
async fn attachment_edits_on_stale_rev_conflict(fx: Fx) {
    let db = fx.db();
    let r1 = write(db, serde_json::json!({"_id": "d"})).await;
    let r2 = db
        .put_attachment("d", "a", &r1, b"one".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let r3 = db
        .put_attachment("d", "b", &r2, b"bee".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let seq = db.info().await.unwrap().update_seq;
    // Same answers as CouchDB: 409 for a stale revision, 404 when the
    // revision never had the attachment.
    assert!(matches!(
        db.put_attachment("d", "a", &r1, b"two".to_vec(), "text/plain")
            .await,
        Err(RouchError::Conflict)
    ));
    assert!(matches!(
        db.remove_attachment("d", "a", &r2).await,
        Err(RouchError::Conflict)
    ));
    assert!(matches!(
        db.remove_attachment("d", "a", &r1).await,
        Err(RouchError::NotFound(_))
    ));
    assert_eq!(db.info().await.unwrap().update_seq, seq);
    assert_eq!(db.get("d").await.unwrap().rev.unwrap().to_string(), r3);
    assert_eq!(db.get_attachment("d", "a").await.unwrap(), b"one");
    assert_eq!(db.get_attachment("d", "b").await.unwrap(), b"bee");
}

/// `bulk_get` answers every requested item in order: the winner, a
/// specific revision (with its ancestry), or a `not_found` error.
async fn bulk_get_reports_each_item(mut fx: Fx) {
    let r1 = write(fx.db(), serde_json::json!({"_id": "d", "v": 1})).await;
    let r2 = write(fx.db(), serde_json::json!({"_id": "d", "_rev": r1, "v": 2})).await;
    fx.reopen();
    let unknown = format!("9-{}", hash32('9'));
    let item = |id: &str, rev: Option<&str>| BulkGetItem {
        id: id.into(),
        rev: rev.map(String::from),
    };
    let res = fx
        .db()
        .adapter()
        .bulk_get(vec![
            item("d", None),
            item("missing", None),
            item("d", Some(&r1)),
            item("d", Some(&unknown)),
        ])
        .await
        .unwrap();
    let ids: Vec<&str> = res.results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["d", "missing", "d", "d"]);
    assert!(res.results.iter().all(|r| r.docs.len() == 1));
    let ok = |i: usize| res.results[i].docs[0].ok.clone();
    let err = |i: usize| {
        let e = res.results[i].docs[0].error.as_ref().unwrap();
        (e.id.clone(), e.error.clone())
    };
    assert_eq!(
        ok(0),
        Some(serde_json::json!({
            "_id": "d", "_rev": r2, "v": 2,
            "_revisions": {"start": 2, "ids": [hash_of(&r2), hash_of(&r1)]}
        }))
    );
    assert_eq!(err(1), ("missing".to_string(), "not_found".to_string()));
    assert!(res.results[1].docs[0].ok.is_none());
    assert_eq!(
        ok(2),
        Some(serde_json::json!({
            "_id": "d", "_rev": r1, "v": 1,
            "_revisions": {"start": 1, "ids": [hash_of(&r1)]}
        }))
    );
    assert_eq!(err(3), ("d".to_string(), "not_found".to_string()));
}

conformance!(writes:
    bulk_docs_generates_missing_ids,
    attachment_edits_on_stale_rev_conflict,
    bulk_get_reports_each_item,
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
    // Both write modes report it on the document, with the same reason as
    // `put` (and as the http adapter).
    let too_deep = format!("Document nesting exceeds the maximum depth of {MAX_NESTING_DEPTH}");
    let mut over = nested(MAX_NESTING_DEPTH + 1);
    over["_id"] = "over".into();
    let res = fx
        .db()
        .bulk_docs(vec![doc(over.clone())], BulkDocsOptions::new())
        .await
        .unwrap();
    assert_eq!(
        (res[0].error.as_deref(), res[0].reason.as_deref()),
        (Some("bad_request"), Some(too_deep.as_str()))
    );
    over["_rev"] = format!("1-{}", hash32('a')).into();
    let res = write_replicated(fx.db(), over).await;
    assert!(!res.ok);
    assert_eq!(
        (res.error.as_deref(), res.reason.as_deref()),
        (Some("bad_request"), Some(too_deep.as_str()))
    );
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
    let ddoc = || DesignDocument::new("app");
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

// === section: f55 ===

/// F55: a design document goes through `get_design` + `put_design`
/// unchanged, whatever it holds (`views.lib`, Mango index views, view and
/// ddoc `options`, object-valued functions, custom fields, attachments), and
/// survives a reopen; the server's Mango indexes read it back.
async fn design_doc_round_trip_is_lossless(mut fx: Fx) {
    let raw = serde_json::json!({
        "language": "query",
        "views": {
            "lib": {"util": "exports.x = 1;"},
            "by-age": {"map": {"fields": {"age": "asc"}, "partial_filter_selector": {}},
                "reduce": "_count", "options": {"def": {"fields": ["age"]}}},
            "js": {"map": "function(doc){ emit(doc._id); }", "options": {"local_seq": true}}
        },
        "filters": {"f": "function(doc){ return true; }", "erl": {"src": "x"}},
        "options": {"partitioned": false},
        "autoupdate": false,
        "rewrites": [{"from": "/a", "to": "/b"}],
        "custom": {"n": [1, 2.5, null, true, "s"]},
        "_attachments": {"a.txt": {"content_type": "text/plain", "data": "aGk="}}
    });
    let r1 = fx
        .db()
        .put("_design/app", raw.clone())
        .await
        .unwrap()
        .rev
        .unwrap();
    let ddoc = fx.db().get_design("app").await.unwrap();
    assert_eq!(ddoc.rev.as_deref(), Some(r1.as_str()));
    assert!(ddoc.extra["_attachments"]["a.txt"]["stub"] == true);
    let r2 = fx.db().put_design(ddoc).await.unwrap().rev.unwrap();
    assert_eq!(generation(&r2), 2);
    fx.reopen();
    let db = fx.db();
    let stored = db.get("_design/app").await.unwrap();
    let mut expected = raw.clone();
    expected.as_object_mut().unwrap().remove("_attachments");
    assert_eq!(stored.data, expected);
    assert_eq!(stored.attachments["a.txt"].length, 2);
    assert_eq!(
        db.get_attachment("_design/app", "a.txt").await.unwrap(),
        b"hi"
    );
    let mut again = db.get_design("app").await.unwrap().to_json();
    let obj = again.as_object_mut().unwrap();
    assert_eq!(obj.remove("_id"), Some(serde_json::json!("_design/app")));
    assert_eq!(obj.remove("_rev"), Some(serde_json::json!(r2)));
    assert_eq!(obj.remove("_attachments").unwrap()["a.txt"]["stub"], true);
    assert_eq!(again, expected);
}

conformance!(f55: design_doc_round_trip_is_lossless);

// === section: revpos ===

/// Attachment `revpos` is the generation of the revision that uploaded the
/// data, as in CouchDB 3.5.1 (the same writes as `attachments_match_couchdb`
/// in `replication.rs`): stubs, body edits and a reopen keep it, a
/// standalone upload and a re-upload of identical bytes set it, and
/// `bulk_get` and replication carry it.
async fn attachment_revpos_follows_couchdb(mut fx: Fx) {
    let hello = serde_json::json!({"content_type": "application/octet-stream", "data": "aGVsbG8="});
    let stub = serde_json::json!({"stub": true});
    let db = fx.db();
    let r1 = db
        .put(
            "d",
            serde_json::json!({"v": 1, "_attachments": {"a.bin": hello}}),
        )
        .await
        .unwrap()
        .rev
        .unwrap();
    let r2 = db
        .update(
            "d",
            &r1,
            serde_json::json!({"v": 2, "_attachments": {"a.bin": stub}}),
        )
        .await
        .unwrap()
        .rev
        .unwrap();
    let r3 = db
        .put_attachment(
            "d",
            "b.bin",
            &r2,
            b"xyz".to_vec(),
            "application/octet-stream",
        )
        .await
        .unwrap()
        .rev
        .unwrap();
    db.update(
        "d",
        &r3,
        serde_json::json!({"v": 4, "_attachments": {"a.bin": hello, "b.bin": stub}}),
    )
    .await
    .unwrap();
    fx.reopen();
    let db = fx.db();

    let stub_json = |revpos: u64, data: &[u8]| {
        serde_json::json!({"content_type": "application/octet-stream", "revpos": revpos,
            "digest": attachment_digest(data), "length": data.len(), "stub": true})
    };
    let expected =
        serde_json::json!({"a.bin": stub_json(4, b"hello"), "b.bin": stub_json(3, b"xyz")});
    assert_eq!(
        db.get("d").await.unwrap().to_json()["_attachments"],
        expected
    );
    let old = get_rev(db, "d", &r2).await.unwrap();
    assert_eq!(
        old.to_json()["_attachments"],
        serde_json::json!({"a.bin": stub_json(1, b"hello")})
    );

    let got = db
        .adapter()
        .bulk_get(vec![BulkGetItem {
            id: "d".into(),
            rev: None,
        }])
        .await
        .unwrap();
    let doc = got.results[0].docs[0].ok.as_ref().unwrap();
    assert_eq!(doc["_attachments"]["a.bin"]["revpos"], 4);
    assert_eq!(doc["_attachments"]["a.bin"]["data"], "aGVsbG8=");
    assert_eq!(doc["_attachments"]["b.bin"]["revpos"], 3);

    let copy = fx.sibling("copy");
    assert!(db.replicate_to(&copy).await.unwrap().ok);
    assert_eq!(
        copy.get("d").await.unwrap().to_json()["_attachments"],
        expected
    );
    let back = fx.sibling("back");
    assert!(back.replicate_from(&copy).await.unwrap().ok);
    assert_eq!(
        back.get("d").await.unwrap().to_json()["_attachments"],
        expected
    );
}

conformance!(revpos: attachment_revpos_follows_couchdb);

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
