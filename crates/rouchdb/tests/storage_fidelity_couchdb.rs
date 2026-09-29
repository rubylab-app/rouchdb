//! Storage behaviour pinned against a real CouchDB (see `common`): what the
//! local adapters were fixed to do must also be what the http adapter does
//! against CouchDB, so the three adapters agree.

mod common;

use std::collections::HashMap;

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::{
    AllDocsOptions, BulkDocsOptions, BulkGetItem, ChangesOptions, Database, DesignDocument,
    Document, FindOptions, GetOptions, MAX_NESTING_DEPTH, RouchError, SecurityDocument,
};

/// `{"v": [[...[1]...]]}` crossing `depth` containers (top-level included).
fn nested_text(depth: usize) -> String {
    format!(
        r#"{{"v":{}1{}}}"#,
        "[".repeat(depth - 1),
        "]".repeat(depth - 1)
    )
}

/// Store a raw JSON document straight in CouchDB (not through rouchdb).
async fn couch_put(url: &str, id: &str, body: String) {
    let resp = reqwest::Client::new()
        .put(format!("{url}/{id}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
}

fn hash32(c: char) -> String {
    std::iter::repeat_n(c, 32).collect()
}

/// Q-API-3: CouchDB accepts deeply nested documents; rouchdb reads them
/// over http (get, changes, all_docs, bulk_get) and replicates them, up to
/// its own limit. A deeper one is a clear per-request error, not a crash.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn deep_couchdb_documents_are_readable_and_replicate() {
    let url = fresh_remote_db("sfid_deep").await;
    couch_put(&url, "deep", nested_text(300)).await;
    couch_put(&url, "edge", nested_text(MAX_NESTING_DEPTH)).await;
    let expected = nested(300);
    let remote = Database::http(&url);

    assert_eq!(remote.get("deep").await.unwrap().data, expected);
    let changes = remote
        .changes(ChangesOptions {
            include_docs: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(changes.results.len(), 2);
    let all = remote
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert!(all.rows.iter().all(|r| r.doc.is_some()));
    let bulk = remote
        .adapter()
        .bulk_get(vec![BulkGetItem {
            id: "edge".into(),
            rev: None,
        }])
        .await
        .unwrap();
    assert!(bulk.results[0].docs[0].ok.is_some());
    // Mango on the server (a raw request) returns them too.
    let found = remote
        .find(FindOptions {
            selector: serde_json::json!({"v": {"$type": "array"}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.docs.len(), 2);
    assert!(found.docs.iter().any(|d| d["v"] == expected["v"]));

    let dir = tempfile::tempdir().unwrap();
    let local = Database::open(dir.path().join("deep.redb"), "deep").unwrap();
    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(result.docs_written, 2);
    assert_eq!(local.get("deep").await.unwrap().data, expected);
    // And back: a copy pushed to a fresh CouchDB database is identical.
    let url2 = fresh_remote_db("sfid_deep_back").await;
    let back = Database::http(&url2);
    let result = local.replicate_to(&back).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(back.get("deep").await.unwrap().data, expected);

    couch_put(&url, "over", nested_text(MAX_NESTING_DEPTH + 20)).await;
    let err = remote.get("over").await.unwrap_err();
    assert!(err.to_string().contains("exceeds the maximum"), "{err}");

    delete_remote_db(&url).await;
    delete_remote_db(&url2).await;
}

/// The value of [`nested_text`], built without parsing.
fn nested(depth: usize) -> serde_json::Value {
    let mut v = serde_json::json!(1);
    for _ in 1..depth {
        v = serde_json::Value::Array(vec![v]);
    }
    serde_json::json!({ "v": v })
}

/// Q-CORE-8: over http, a malformed revision is `InvalidRev` and `remove`
/// of a missing or deleted document is `NotFound` (without writing a
/// tombstone), like the local adapters.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn missing_documents_and_bad_revisions_over_http() {
    let url = fresh_remote_db("sfid_missing").await;
    let db = Database::http(&url);
    let r1 = db.put("d", serde_json::json!({"v": 1})).await.unwrap();
    let r1 = r1.rev.unwrap();
    for bad in ["not-a-rev", "abc"] {
        let got = db
            .get_with_opts(
                "d",
                GetOptions {
                    rev: Some(bad.into()),
                    ..Default::default()
                },
            )
            .await;
        assert!(
            matches!(got, Err(RouchError::InvalidRev(_))),
            "{bad}: {got:?}"
        );
    }
    // (An *update* of a missing document is left to CouchDB: 3.5.1 answers
    // `conflict` only when the document's shard is empty, and otherwise
    // writes `2-x` on top of a phantom `1-...`. PouchDB and the local
    // adapters always answer `conflict`.)
    let rev = format!("1-{}", hash32('a'));
    assert!(matches!(
        db.remove("nodoc", &rev).await,
        Err(RouchError::NotFound(_))
    ));
    let r2 = db.remove("d", &r1).await.unwrap().rev.unwrap();
    assert!(matches!(
        db.remove("d", &r2).await,
        Err(RouchError::NotFound(_))
    ));
    // Nothing was written by the failed removals.
    let rows = db
        .all_docs(AllDocsOptions {
            keys: Some(vec!["nodoc".into(), "d".into()]),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], rouchdb::AllDocsRow::not_found("nodoc"));
    assert_eq!(rows[1].rev().unwrap(), r2);
    assert!(rows[1].is_deleted());
    // Upper-case revision ids are the same revisions.
    let r = db
        .put("u", serde_json::json!({}))
        .await
        .unwrap()
        .rev
        .unwrap();
    let upper = r.to_uppercase();
    let r2 = db
        .update("u", &upper, serde_json::json!({"v": 2}))
        .await
        .unwrap();
    assert!(r2.rev.unwrap().starts_with("2-"));
    delete_remote_db(&url).await;
}

/// Q-API-4 / Q-API-7 / Q-API-11 over http: `_local/` ids are local
/// documents, a design-document conflict is an error, and a destroyed
/// database is usable again (re-created, empty).
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn local_documents_design_conflicts_and_destroy_over_http() {
    let url = fresh_remote_db("sfid_local").await;
    let db = Database::http(&url);
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
    let got = db.get("_local/x").await.unwrap();
    assert_eq!(got.rev.unwrap().to_string(), "0-2");
    assert_eq!(got.data, serde_json::json!({"v": 2}));
    assert_eq!(db.info().await.unwrap().doc_count, 0);
    assert!(
        db.all_docs(AllDocsOptions::new())
            .await
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(
        db.remove("_local/x", "0-2").await.unwrap().rev.as_deref(),
        Some("0-0")
    );
    assert!(matches!(
        db.remove("_local/x", "0-2").await,
        Err(RouchError::NotFound(_))
    ));

    let ddoc = || DesignDocument::new("app");
    assert!(db.put_design(ddoc()).await.unwrap().ok);
    assert!(matches!(
        db.put_design(ddoc()).await,
        Err(RouchError::Conflict)
    ));

    db.adapter()
        .put_local("checkpoint", serde_json::json!({"last_seq": 1}))
        .await
        .unwrap();
    let mut sec = SecurityDocument::default();
    sec.members.roles.push("team".into());
    db.put_security(sec).await.unwrap();
    assert_eq!(db.get_security().await.unwrap().members.roles, ["team"]);
    db.destroy().await.unwrap();
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.doc_del_count), (0, 0));
    assert!(matches!(
        db.adapter().get_local("checkpoint").await,
        Err(RouchError::NotFound(_))
    ));
    db.put("again", serde_json::json!({})).await.unwrap();
    db.destroy().await.unwrap();
    db.destroy().await.unwrap();
    delete_remote_db(&url).await;
}

/// Q-CORE-10 / Q-CORE-11: `revs_diff` and stub handling give the same
/// answers on memory as on CouchDB.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn revs_diff_and_stubs_match_couchdb() {
    let url = fresh_remote_db("sfid_diff").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");
    let (x, a, b, e, f) = (
        hash32('1'),
        hash32('a'),
        hash32('b'),
        hash32('e'),
        hash32('f'),
    );
    let docs = [
        serde_json::json!({"_id": "rd", "_rev": format!("1-{x}")}),
        serde_json::json!({"_id": "rd", "_rev": format!("2-{a}"), "_revisions": {"start": 2, "ids": [a, x]}}),
        serde_json::json!({"_id": "rd", "_rev": format!("2-{b}"), "_deleted": true, "_revisions": {"start": 2, "ids": [b, x]}}),
        serde_json::json!({"_id": "rd", "_rev": format!("3-{e}"), "_revisions": {"start": 3, "ids": [e, e, x]}}),
    ];
    for db in [&remote, &local] {
        for json in &docs {
            let doc = Document::from_json(json.clone()).unwrap();
            db.adapter()
                .bulk_docs(vec![doc], BulkDocsOptions::replication())
                .await
                .unwrap();
        }
    }
    let queries = [
        vec![format!("3-{f}"), format!("4-{f}"), format!("2-{f}")],
        vec![format!("3-{f}")],
        vec![format!("2-{f}"), format!("1-{x}")],
        vec![format!("4-{f}"), format!("4-{f}")],
        vec![format!("3-{}", f.to_uppercase()), format!("3-{e}")],
    ];
    for revs in queries {
        let req = HashMap::from([("rd".to_string(), revs.clone())]);
        let want = remote.adapter().revs_diff(req.clone()).await.unwrap();
        let got = local.adapter().revs_diff(req).await.unwrap();
        let pair = |r: &rouchdb::RevsDiffResponse| {
            r.results
                .get("rd")
                .map(|d| (d.missing.clone(), d.possible_ancestors.clone()))
        };
        assert_eq!(pair(&got), pair(&want), "{revs:?}");
    }

    // Stubs: matched by name, stored metadata wins, renamed = missing_stub.
    for db in [&remote, &local] {
        let r1 = db
            .put(
                "att",
                serde_json::json!({"_attachments": {"a.txt": {"content_type": "text/plain", "data": "SGVsbG8="}}}),
            )
            .await
            .unwrap()
            .rev
            .unwrap();
        let r2 = db
            .update(
                "att",
                &r1,
                serde_json::json!({"v": 1, "_attachments": {"a.txt": {
                    "stub": true, "digest": "md5-AAAAAAAAAAAAAAAAAAAAAA==", "content_type": "image/png"
                }}}),
            )
            .await
            .unwrap()
            .rev
            .unwrap();
        let got = db.get("att").await.unwrap();
        assert_eq!(got.attachments["a.txt"].content_type, "text/plain");
        assert_eq!(db.get_attachment("att", "a.txt").await.unwrap(), b"Hello");
        let digest = got.attachments["a.txt"].digest.clone();
        let renamed = db
            .update(
                "att",
                &r2,
                serde_json::json!({"_attachments": {"b.txt": {"stub": true, "digest": digest}}}),
            )
            .await;
        assert!(
            matches!(renamed, Err(RouchError::BadRequest(ref r)) if r.contains("b.txt")),
            "{renamed:?}"
        );
    }
    delete_remote_db(&url).await;
}

/// `changes` with `limit: 0` is empty over http too (CouchDB 3.5.1), like
/// the local adapters.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn changes_limit_zero_over_http() {
    let url = fresh_remote_db("sfid_changes").await;
    let db = Database::http(&url);
    db.put("a", serde_json::json!({})).await.unwrap();
    let res = db
        .changes(ChangesOptions {
            limit: Some(0),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(res.results.is_empty(), "{res:?}");
    delete_remote_db(&url).await;
}

/// The current JSON of a document, read straight from CouchDB.
async fn couch_get(url: &str, id: &str) -> serde_json::Value {
    let resp = reqwest::get(format!("{url}/{id}")).await.unwrap();
    assert!(resp.status().is_success(), "{id}: {}", resp.status());
    resp.json().await.unwrap()
}

/// F55: design documents written to CouchDB by other clients (Fauxton-style
/// PUTs with `views.lib`, options and custom members, a standalone
/// attachment, Mango `_index`) go through `get_design` + `put_design`
/// unchanged (attachment stubs and their `revpos` included), over http and
/// through a local database replicated from and back to CouchDB.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn couchdb_design_documents_round_trip() {
    let url = fresh_remote_db("sfid_ddocs").await;
    couch_put(
        &url,
        "_design/app",
        serde_json::json!({
            "language": "javascript",
            "views": {
                "lib": {"util": "exports.twice = function(x){ return 2*x; };"},
                "by_type": {"map": "function (doc) {\n  emit(doc.type, 1);\n}",
                    "reduce": "_count", "options": {"collation": "raw"}},
                "by_n": {"map": "function(doc){ emit(require('views/lib/util').twice(doc.n)); }"}
            },
            "filters": {"users": "function(doc, req){ return doc.type === 'user'; }",
                "erl": {"src": "x"}},
            "validate_doc_update": "function(newDoc, oldDoc, userCtx){ }",
            "options": {"partitioned": false, "local_seq": true},
            "autoupdate": false,
            "rewrites": [{"from": "/a", "to": "/b"}],
            "custom": {"nested": [1, 2.5, "x", null, true]}
        })
        .to_string(),
    )
    .await;
    // An attachment (binary: CouchDB keeps it uncompressed, so its digest
    // is the one rouchdb computes too).
    let rev = couch_get(&url, "_design/app").await["_rev"].clone();
    let resp = reqwest::Client::new()
        .put(format!(
            "{url}/_design/app/logo.bin?rev={}",
            rev.as_str().unwrap()
        ))
        .header("Content-Type", "application/octet-stream")
        .body(vec![0u8, 1, 2, 255])
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let resp = reqwest::Client::new()
        .post(format!("{url}/_index"))
        .json(&serde_json::json!({"index": {"fields": ["t"]}, "ddoc": "mango", "name": "by-t"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let ids = ["_design/app", "_design/mango"];

    let remote = Database::http(&url);
    for id in ids {
        let before = couch_get(&url, id).await;
        let ddoc = remote.get_design(id).await.unwrap();
        assert_eq!(ddoc.to_json(), before, "{id}");
        let rev = remote.put_design(ddoc).await.unwrap().rev.unwrap();
        let mut after = couch_get(&url, id).await;
        assert_eq!(after["_rev"], rev, "{id}");
        after["_rev"] = before["_rev"].clone();
        assert_eq!(after, before, "{id}");
    }

    // Replicated into a local database, edited there and pushed back.
    let local = Database::memory("ddocs");
    assert!(local.replicate_from(&remote).await.unwrap().ok);
    for id in ids {
        assert_eq!(
            local.get_design(id).await.unwrap().to_json(),
            couch_get(&url, id).await,
            "{id}"
        );
    }
    let mut ddoc = local.get_design("app").await.unwrap();
    ddoc.extra
        .insert("custom".into(), serde_json::json!("edited"));
    ddoc.other_views.remove("lib");
    local.put_design(ddoc.clone()).await.unwrap();
    assert!(local.replicate_to(&remote).await.unwrap().ok);
    let mut pushed = couch_get(&url, "_design/app").await;
    let mut expected = local.get_design("app").await.unwrap().to_json();
    assert_eq!(pushed["_rev"], expected["_rev"]);
    assert_eq!(pushed["custom"], "edited");
    assert!(pushed["views"].get("lib").is_none());
    assert_eq!(pushed["_attachments"]["logo.bin"]["revpos"], 2);
    pushed["_rev"] = serde_json::Value::Null;
    expected["_rev"] = serde_json::Value::Null;
    assert_eq!(pushed, expected);
}
