//! Files written by the rouchdb 0.4 release, read by this version.
//!
//! 0.4 cannot read what 0.5 writes (it would treat 0.5 document records as
//! missing and silently replace their history on the next write), so:
//! - 0.5 refuses 0.4 files unless asked to upgrade them, without touching them;
//! - the upgrade keeps every document, revision body, attachment and local
//!   document, after a backup that 0.4 still opens;
//! - 0.4 refuses upgraded files and files created by 0.5, writing nothing.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use rouchdb_adapter_redb::{OpenOptions, RedbAdapter, StoredFormat, UpgradePolicy};
use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::{self as doc05, attachment_digest};
use rouchdb_core::error::RouchError;
use rouchdb04_core::adapter::Adapter as Adapter04;
use rouchdb04_core::document as doc04;
use rouchdb04_redb::RedbAdapter as RedbAdapter04;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Logical snapshots of a file (redb rewrites its header on every open, so
// "unchanged" means same tables and same entries, not same bytes)
// ---------------------------------------------------------------------------

/// The value type of the format guard, by name (redb matches types by
/// name): if this stops opening 0.5 files, the guard changed.
#[derive(Debug)]
struct Guard;

impl redb::Value for Guard {
    type SelfType<'a>
        = Guard
    where
        Self: 'a;
    type AsBytes<'a>
        = [u8; 0]
    where
        Self: 'a;
    fn fixed_width() -> Option<usize> {
        Some(0)
    }
    fn from_bytes<'a>(_: &'a [u8]) -> Guard
    where
        Self: 'a,
    {
        Guard
    }
    fn as_bytes<'a, 'b: 'a>(_: &'a Guard) -> [u8; 0]
    where
        Self: 'b,
    {
        []
    }
    fn type_name() -> redb::TypeName {
        redb::TypeName::new("rouchdb-format-2 (this file requires rouchdb >= 0.5)")
    }
}

type Snapshot = BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>>;

fn snapshot(path: &Path) -> Snapshot {
    use redb::{ReadableTable, TableHandle};
    fn rows<K: redb::Key + 'static, V: redb::Value + 'static>(
        txn: &redb::ReadTransaction,
        name: &str,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let table = txn
            .open_table(redb::TableDefinition::<K, V>::new(name))
            .unwrap();
        table
            .iter()
            .unwrap()
            .map(|e| {
                let (k, v) = e.unwrap();
                (
                    K::as_bytes(&k.value()).as_ref().to_vec(),
                    V::as_bytes(&v.value()).as_ref().to_vec(),
                )
            })
            .collect()
    }
    let db = redb::Database::open(path).unwrap();
    let txn = db.begin_read().unwrap();
    let names: Vec<String> = txn
        .list_tables()
        .unwrap()
        .map(|t| t.name().to_string())
        .collect();
    let guarded = txn
        .open_table(redb::TableDefinition::<&str, Guard>::new("metadata"))
        .is_ok();
    names
        .into_iter()
        .map(|name| {
            let r = match name.as_str() {
                "changes" => rows::<u64, &[u8]>(&txn, &name),
                "metadata" if guarded => rows::<&str, Guard>(&txn, &name),
                _ => rows::<&str, &[u8]>(&txn, &name),
            };
            (name, r)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The data 0.4 writes
// ---------------------------------------------------------------------------

/// What the test wrote, to read it back: every revision with a stored body,
/// every (document, revision, attachment) and every local document.
#[derive(Default)]
struct Written {
    docs: BTreeMap<String, Vec<String>>,
    atts: Vec<(String, String, String)>,
    locals: Vec<String>,
}

impl Written {
    fn rev(&mut self, id: &str, rev: &str) {
        self.docs.entry(id.into()).or_default().push(rev.into());
    }
}

fn doc04(id: &str, rev: Option<&str>, data: Value, deleted: bool) -> doc04::Document {
    doc04::Document {
        id: id.into(),
        rev: rev.map(|r| r.parse().unwrap()),
        deleted,
        data,
        attachments: HashMap::new(),
    }
}

async fn put04(db: &RedbAdapter04, id: &str, rev: Option<&str>, data: Value) -> String {
    let r = db
        .bulk_docs(
            vec![doc04(id, rev, data, false)],
            doc04::BulkDocsOptions::new(),
        )
        .await
        .unwrap();
    assert!(r[0].ok, "{:?}", r[0]);
    r[0].rev.clone().unwrap()
}

async fn replicate04(db: &RedbAdapter04, id: &str, rev: &str, data: Value) {
    let r = db
        .bulk_docs(
            vec![doc04(id, Some(rev), data, false)],
            doc04::BulkDocsOptions::replication(),
        )
        .await
        .unwrap();
    assert!(r[0].ok, "{:?}", r[0]);
}

const UPPER: &str = "ABCDEF0123456789ABCDEF0123456789";

/// Realistic 0.4 data: histories, conflicts, attachments (shared bytes, a
/// re-attached name, one only an old revision has), deleted documents,
/// local documents both ways, an upper-case replicated revision.
async fn write_with_0_4(path: &Path) -> Written {
    let db = RedbAdapter04::open(path, "compat").unwrap();
    let mut w = Written::default();

    let mut rev = None;
    for v in 1..=4 {
        let r = put04(
            &db,
            "hist",
            rev.as_deref(),
            json!({"v": v, "nested": {"list": [v, "x"]}}),
        )
        .await;
        w.rev("hist", &r);
        rev = Some(r);
    }

    let base = put04(&db, "conf", None, json!({"side": "base"})).await;
    w.rev("conf", &base);
    let base_hash = base.split_once('-').unwrap().1.to_string();
    for (hash, side) in [("a".repeat(32), "left"), ("b".repeat(32), "right")] {
        let rev = format!("2-{hash}");
        replicate04(
            &db,
            "conf",
            &rev,
            json!({"side": side, "_revisions": {"start": 2, "ids": [hash, base_hash]}}),
        )
        .await;
        w.rev("conf", &rev);
    }

    let r1 = put04(&db, "att", None, json!({"kind": "files"})).await;
    let r2 = db
        .put_attachment("att", "a.txt", &r1, b"shared bytes".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let r3 = db
        .put_attachment(
            "att",
            "b.bin",
            &r2,
            vec![0, 1, 2, 255],
            "application/octet-stream",
        )
        .await
        .unwrap()
        .rev
        .unwrap();
    for r in [&r1, &r2, &r3] {
        w.rev("att", r);
    }
    w.atts.push(("att".into(), r2.clone(), "a.txt".into()));
    w.atts.push(("att".into(), r3.clone(), "a.txt".into()));
    w.atts.push(("att".into(), r3.clone(), "b.bin".into()));

    // Same bytes in another document: one content-addressed copy after the
    // upgrade.
    let s1 = put04(&db, "att-shared", None, json!({})).await;
    let s2 = db
        .put_attachment(
            "att-shared",
            "same.txt",
            &s1,
            b"shared bytes".to_vec(),
            "text/plain",
        )
        .await
        .unwrap()
        .rev
        .unwrap();
    w.rev("att-shared", &s1);
    w.rev("att-shared", &s2);
    w.atts.push(("att-shared".into(), s2, "same.txt".into()));

    // 0.4 keyed bytes by (document, name): re-attaching the name overwrote
    // the bytes of the older revision (already lost in 0.4).
    let t1 = put04(&db, "reatt", None, json!({})).await;
    let t2 = db
        .put_attachment("reatt", "r.txt", &t1, b"version one".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let t3 = db
        .put_attachment("reatt", "r.txt", &t2, b"version two".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    for r in [&t1, &t2, &t3] {
        w.rev("reatt", r);
    }
    w.atts.push(("reatt".into(), t2, "r.txt".into()));
    w.atts.push(("reatt".into(), t3, "r.txt".into()));

    // 0.4 dropped the attachments of a document whose body was updated:
    // only the old revision still has them.
    let o1 = put04(&db, "old-att", None, json!({"v": 1})).await;
    let o2 = db
        .put_attachment(
            "old-att",
            "o.txt",
            &o1,
            b"only in rev 2".to_vec(),
            "text/plain",
        )
        .await
        .unwrap()
        .rev
        .unwrap();
    let o3 = put04(&db, "old-att", Some(&o2), json!({"v": 3})).await;
    for r in [&o1, &o2, &o3] {
        w.rev("old-att", r);
    }
    w.atts.push(("old-att".into(), o2, "o.txt".into()));

    let d1 = put04(&db, "del", None, json!({"soon": "gone"})).await;
    let d2 = db
        .bulk_docs(
            vec![doc04("del", Some(&d1), json!({}), true)],
            doc04::BulkDocsOptions::new(),
        )
        .await
        .unwrap()[0]
        .rev
        .clone()
        .unwrap();
    w.rev("del", &d1);
    w.rev("del", &d2);

    let up = format!("1-{UPPER}");
    replicate04(&db, "upper", &up, json!({"case": "upper"})).await;
    w.rev("upper", &up);

    // A local document through the local API (replication checkpoints)...
    db.put_local("checkpoint", json!({"last_seq": "17", "session": "s"}))
        .await
        .unwrap();
    w.locals.push("checkpoint".into());
    // ... and through bulk_docs, which 0.4 stored as an ordinary document.
    let l1 = put04(&db, "_local/prefs", None, json!({"theme": "light"})).await;
    put04(&db, "_local/prefs", Some(&l1), json!({"theme": "dark"})).await;

    w
}

// ---------------------------------------------------------------------------
// Reading it back, with 0.4 and with 0.5, as comparable facts
// ---------------------------------------------------------------------------

/// Revision ids as 0.5 reports them (32-digit hex ids in lower case).
fn canonical(rev: &str) -> String {
    rev.parse::<doc05::Revision>().unwrap().to_string()
}

fn canonical_data(mut data: Value) -> Value {
    if let Some(list) = data.get_mut("_conflicts").and_then(Value::as_array_mut) {
        let mut revs: Vec<String> = list
            .iter()
            .map(|r| canonical(r.as_str().unwrap()))
            .collect();
        revs.sort();
        *list = revs.into_iter().map(Value::String).collect();
    }
    data
}

fn att_fact(name: &str, content_type: &str, digest: &str, length: u64) -> Value {
    json!({"name": name, "content_type": content_type, "digest": digest, "length": length})
}

/// Attachment bytes, or "lost" when the stored bytes are not the ones the
/// revision references (0.4 returned the bytes of a later revision then).
fn bytes_fact(digest: &str, bytes: Option<Vec<u8>>) -> Value {
    match bytes {
        Some(b) if attachment_digest(&b) == digest => json!(String::from_utf8_lossy(&b)),
        _ => json!("lost"),
    }
}

async fn facts_0_4(db: &RedbAdapter04, w: &Written) -> Value {
    let mut docs = BTreeMap::new();
    for (id, revs) in &w.docs {
        let winner = match db
            .get(
                id,
                doc04::GetOptions {
                    conflicts: true,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(d) => {
                json!({"rev": canonical(&d.rev.unwrap().to_string()), "data": canonical_data(d.data)})
            }
            Err(e) => json!(format!("{e}").starts_with("not found")),
        };
        let mut bodies = BTreeMap::new();
        for rev in revs {
            let d = db
                .get(
                    id,
                    doc04::GetOptions {
                        rev: Some(rev.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let mut atts: Vec<Value> = d
                .attachments
                .iter()
                .map(|(n, a)| att_fact(n, &a.content_type, &a.digest, a.length))
                .collect();
            atts.sort_by_key(|a| a["name"].as_str().unwrap().to_string());
            bodies.insert(
                canonical(rev),
                json!({"data": d.data, "deleted": d.deleted, "attachments": atts}),
            );
        }
        docs.insert(id.clone(), json!({"winner": winner, "revs": bodies}));
    }
    let mut atts = Vec::new();
    for (id, rev, name) in &w.atts {
        let d = db
            .get(
                id,
                doc04::GetOptions {
                    rev: Some(rev.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let digest = d.attachments[name].digest.clone();
        let bytes = db
            .get_attachment(
                id,
                name,
                doc04::GetAttachmentOptions {
                    rev: Some(rev.clone()),
                },
            )
            .await
            .ok();
        atts.push(json!([
            id,
            canonical(rev),
            name,
            bytes_fact(&digest, bytes)
        ]));
    }
    let mut locals = BTreeMap::new();
    for id in &w.locals {
        locals.insert(id.clone(), db.get_local(id).await.unwrap());
    }
    let all = db.all_docs(doc04::AllDocsOptions::new()).await.unwrap();
    let rows: Vec<Value> = all
        .rows
        .iter()
        .filter(|r| !r.id.starts_with("_local/"))
        .map(|r| json!([r.id, canonical(&r.value.rev)]))
        .collect();
    let changes = db.changes(doc04::ChangesOptions::default()).await.unwrap();
    let mut changed: Vec<String> = changes
        .results
        .iter()
        .map(|c| c.id.clone())
        .filter(|id| !id.starts_with("_local/"))
        .collect();
    changed.sort();
    json!({"docs": docs, "attachments": atts, "locals": locals, "all_docs": rows, "changes": changed})
}

async fn facts_0_5(db: &RedbAdapter, w: &Written) -> Value {
    let mut docs = BTreeMap::new();
    for (id, revs) in &w.docs {
        let winner = match db
            .get(
                id,
                doc05::GetOptions {
                    conflicts: true,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(d) => json!({"rev": d.rev.unwrap().to_string(), "data": canonical_data(d.data)}),
            Err(e) => json!(matches!(e, RouchError::NotFound(_))),
        };
        let mut bodies = BTreeMap::new();
        for rev in revs {
            let d = db
                .get(
                    id,
                    doc05::GetOptions {
                        rev: Some(rev.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let mut atts: Vec<Value> = d
                .attachments
                .iter()
                .map(|(n, a)| att_fact(n, &a.content_type, &a.digest, a.length))
                .collect();
            atts.sort_by_key(|a| a["name"].as_str().unwrap().to_string());
            bodies.insert(
                canonical(rev),
                json!({"data": d.data, "deleted": d.deleted, "attachments": atts}),
            );
        }
        docs.insert(id.clone(), json!({"winner": winner, "revs": bodies}));
    }
    let mut atts = Vec::new();
    for (id, rev, name) in &w.atts {
        let d = db
            .get(
                id,
                doc05::GetOptions {
                    rev: Some(rev.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let digest = d.attachments[name].digest.clone();
        let bytes = db
            .get_attachment(
                id,
                name,
                doc05::GetAttachmentOptions {
                    rev: Some(rev.clone()),
                },
            )
            .await
            .ok();
        atts.push(json!([
            id,
            canonical(rev),
            name,
            bytes_fact(&digest, bytes)
        ]));
    }
    let mut locals = BTreeMap::new();
    for id in &w.locals {
        locals.insert(id.clone(), db.get_local(id).await.unwrap());
    }
    let all = db.all_docs(doc05::AllDocsOptions::new()).await.unwrap();
    let rows: Vec<Value> = all
        .rows
        .iter()
        .map(|r| json!([r.id.clone().unwrap(), r.value.as_ref().unwrap().rev.clone()]))
        .collect();
    let changes = db.changes(doc05::ChangesOptions::default()).await.unwrap();
    let mut changed: Vec<String> = changes.results.iter().map(|c| c.id.clone()).collect();
    changed.sort();
    json!({"docs": docs, "attachments": atts, "locals": locals, "all_docs": rows, "changes": changed})
}

fn open_0_4(path: &Path) -> Result<RedbAdapter04, String> {
    RedbAdapter04::open(path, "compat").map_err(|e| e.to_string())
}

/// 0.4 must fail to open the file, saying which version it needs, and
/// leave it exactly as it was.
fn assert_0_4_refuses(path: &Path) {
    let before = snapshot(path);
    let err = open_0_4(path).err().expect("0.4 must not open a 0.5 file");
    assert!(err.contains("requires rouchdb >= 0.5"), "{err}");
    assert_eq!(snapshot(path), before, "0.4 wrote to a 0.5 file");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_0_4_file_is_refused_then_upgraded_with_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.redb");
    let written = write_with_0_4(&path).await;

    let facts_04 = {
        let db = open_0_4(&path).unwrap();
        facts_0_4(&db, &written).await
    };
    let local_prefs_04 = {
        let db = open_0_4(&path).unwrap();
        db.get("_local/prefs", doc04::GetOptions::default())
            .await
            .unwrap()
    };
    // Sanity of the fixture: the lost bytes are really lost in 0.4 already.
    assert_eq!(
        facts_04["attachments"][4][3], "lost",
        "{:#}",
        facts_04["attachments"]
    );
    let original = snapshot(&path);

    // 1. 0.5 refuses the file and changes nothing: 0.4 still reads
    //    everything, attachments included.
    for opts in [None, Some(OpenOptions::new())] {
        let err = match opts {
            None => RedbAdapter::open(&path, "compat").err(),
            Some(o) => RedbAdapter::open_with(&path, "compat", o).err(),
        }
        .expect("0.5 must refuse a 0.4 file");
        assert!(matches!(err, RouchError::UpgradeRequired { .. }), "{err}");
        assert!(err.to_string().contains("rouchdb migrate"), "{err}");
    }
    assert_eq!(snapshot(&path), original);
    {
        let db = open_0_4(&path).unwrap();
        assert_eq!(facts_0_4(&db, &written).await, facts_04);
    }
    assert_eq!(snapshot(&path), original);

    // 2. A dry run reports without changing anything: it only reads, so
    //    not a byte of the file changes.
    let raw = std::fs::read(&path).unwrap();
    let dry = RedbAdapter::inspect_upgrade(&path).unwrap();
    assert!(!dry.upgraded);
    assert!(
        std::fs::read(&path).unwrap() == raw,
        "the dry run modified the file"
    );
    assert_eq!(snapshot(&path), original);

    // 3. The upgrade, with the default backup.
    let report = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None)).unwrap();
    let backup = dir.path().join("app.redb.rouchdb-0.4.bak");
    assert_eq!(report.backup.as_deref(), Some(backup.as_path()));
    assert_eq!(report.from, StoredFormat::Legacy);
    assert!(report.upgraded);
    // hist conf att att-shared reatt old-att upper live, del deleted.
    assert_eq!((report.doc_count, report.doc_del_count), (7, 1));
    assert_eq!(report.local_docs_moved, 1);
    assert_eq!(report.revs_normalized, 1);
    assert_eq!(report.missing_attachment_refs, 1);
    assert_eq!(report.docs_with_missing_attachments, ["reatt"]);
    assert_eq!(report.docs_with_old_only_attachments, ["old-att"]);
    // 4 entries keyed by (doc, name); "shared bytes" twice.
    assert_eq!(report.attachments_rekeyed, 5);
    assert_eq!(report.case_duplicate_revs_merged, 0);
    assert!(report.docs_with_changed_winner.is_empty());
    // The dry run reported exactly what the upgrade did.
    let mut dry = dry;
    dry.upgraded = true;
    dry.backup = report.backup.clone();
    assert_eq!(dry, report);

    // 4. Every document, revision body, attachment and local document reads
    //    the same through 0.5.
    let db = RedbAdapter::open(&path, "compat").unwrap();
    let facts_05 = facts_0_5(&db, &written).await;
    assert_eq!(facts_05, facts_04);
    let prefs = db
        .get("_local/prefs", doc05::GetOptions::default())
        .await
        .unwrap();
    assert_eq!(prefs.data, local_prefs_04.data);
    assert_eq!(prefs.rev.unwrap().to_string(), "0-2");
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (7, 1));
    drop(db);

    // 5. 0.4 refuses the upgraded file and writes nothing to it.
    assert_0_4_refuses(&path);

    // 6. The backup is the original file, and 0.4 reads it identically.
    assert_eq!(snapshot(&backup), original);
    let db = open_0_4(&backup).unwrap();
    assert_eq!(facts_0_4(&db, &written).await, facts_04);
}

/// 0.4 accepted a replicated revision id in upper case, and then the same
/// revision in lower case as a different one. 0.5 merges the two spellings
/// (reporting a body that differs), and reports the documents whose winning
/// revision changes because 0.5 compares the ids in lower case.
#[tokio::test]
async fn case_duplicates_and_winner_changes_written_by_0_4() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("case.redb");
    {
        let db = RedbAdapter04::open(&path, "case").unwrap();
        let lower = UPPER.to_ascii_lowercase();
        replicate04(&db, "dup", &format!("1-{UPPER}"), json!({"from": "upper"})).await;
        replicate04(&db, "dup", &format!("1-{lower}"), json!({"from": "lower"})).await;
        replicate04(&db, "same", &format!("1-{UPPER}"), json!({"v": 1})).await;
        replicate04(&db, "same", &format!("1-{lower}"), json!({"v": 1})).await;
        let b = format!("1-B{}", "0".repeat(31));
        let a = format!("1-a{}", "0".repeat(31));
        replicate04(&db, "win", &b, json!({"side": "b"})).await;
        replicate04(&db, "win", &a, json!({"side": "a"})).await;
        let winner = db.get("win", doc04::GetOptions::default()).await.unwrap();
        assert_eq!(winner.data["side"], "a", "0.4 ranks 'a' above 'B'");
    }
    let raw = std::fs::read(&path).unwrap();
    let dry = RedbAdapter::inspect_upgrade(&path).unwrap();
    assert!(std::fs::read(&path).unwrap() == raw);
    let report = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None)).unwrap();
    assert_eq!(
        dry.case_duplicate_bodies_discarded,
        report.case_duplicate_bodies_discarded
    );
    assert_eq!(report.case_duplicate_revs_merged, 2);
    // 0.4's winner of "dup" was the lower-case spelling (it sorts last):
    // its body is kept, the other one is reported.
    assert_eq!(report.case_duplicate_bodies_discarded.len(), 1);
    let d = &report.case_duplicate_bodies_discarded[0];
    assert_eq!(
        (d.doc_id.as_str(), d.rev.as_str()),
        ("dup", format!("1-{UPPER}").as_str())
    );
    assert_eq!(report.docs_with_changed_winner, ["win"]);

    let db = RedbAdapter::open(&path, "case").unwrap();
    let dup = db.get("dup", doc05::GetOptions::default()).await.unwrap();
    assert_eq!(dup.data["from"], "lower");
    assert_eq!(
        db.get("same", doc05::GetOptions::default())
            .await
            .unwrap()
            .data["v"],
        1
    );
    let win = db.get("win", doc05::GetOptions::default()).await.unwrap();
    assert_eq!(win.data["side"], "b");
    drop(db);
    // The backup still has both bodies, for 0.4.
    let backup = open_0_4(&dir.path().join("case.redb.rouchdb-0.4.bak")).unwrap();
    let old = backup
        .get(
            "dup",
            doc04::GetOptions {
                rev: Some(format!("1-{UPPER}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(old.data["from"], "upper");
}

/// The silent history loss found in review: 0.4 writing to a document 0.5
/// wrote replaced its whole history. Now 0.4 cannot open such a file at all,
/// whether 0.5 created or upgraded it.
#[tokio::test]
async fn a_file_created_by_0_5_is_refused_by_0_4() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.redb");
    let rev = {
        let db = RedbAdapter::open(&path, "new").unwrap();
        let mut rev: Option<String> = None;
        for v in 0..3 {
            let d = doc05::Document {
                id: "a".into(),
                rev: rev.as_deref().map(|r| r.parse().unwrap()),
                deleted: false,
                data: json!({"v": v}),
                attachments: HashMap::new(),
            };
            let r = db
                .bulk_docs(vec![d], doc05::BulkDocsOptions::new())
                .await
                .unwrap();
            rev = r[0].rev.clone();
        }
        db.put_local("ck", json!({"seq": 1})).await.unwrap();
        rev.unwrap()
    };
    assert_0_4_refuses(&path);

    // Destroying keeps the guard.
    {
        let db = RedbAdapter::open(&path, "new").unwrap();
        assert_eq!(
            db.get("a", doc05::GetOptions::default())
                .await
                .unwrap()
                .rev
                .unwrap()
                .to_string(),
            rev
        );
        db.destroy().await.unwrap();
    }
    assert_0_4_refuses(&path);
}

/// Opening in 0.5 with an upgrade policy gives the same result as the
/// explicit upgrade, and 0.4 is locked out afterwards.
#[tokio::test]
async fn open_with_upgrades_and_locks_0_4_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.redb");
    let written = write_with_0_4(&path).await;
    let facts_04 = facts_0_4(&open_0_4(&path).unwrap(), &written).await;

    let backup = dir.path().join("elsewhere.bak");
    let db = RedbAdapter::open_with(
        &path,
        "compat",
        OpenOptions::new().upgrade(UpgradePolicy::WithBackup(Some(backup.clone()))),
    )
    .unwrap();
    assert_eq!(
        db.upgrade_report().unwrap().backup.as_deref(),
        Some(backup.as_path())
    );
    assert_eq!(facts_0_5(&db, &written).await, facts_04);
    drop(db);
    assert_0_4_refuses(&path);
    assert_eq!(
        facts_0_4(&open_0_4(&backup).unwrap(), &written).await,
        facts_04
    );
}
