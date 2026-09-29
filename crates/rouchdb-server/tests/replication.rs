//! F19: the endpoints the CouchDB replication protocol needs, and real
//! replication through `Database::http` in both directions.
mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use rouchdb::{BulkDocsOptions, Database, Document, ReplicationOptions};
use serde_json::json;

fn bad_request(reason: &str) -> serde_json::Value {
    json!({"error": "bad_request", "reason": reason})
}

fn missing() -> serde_json::Value {
    json!({"error": "not_found", "reason": "missing"})
}

/// The hash part of a `N-hash` revision.
fn hash(rev: &str) -> &str {
    rev.split_once('-').unwrap().1
}

async fn doc_with_two_revs(db: &Database) -> (String, String) {
    let r1 = db.put("a", json!({"v": 1})).await.unwrap().rev.unwrap();
    let r2 = db
        .update("a", &r1, json!({"v": 2}))
        .await
        .unwrap()
        .rev
        .unwrap();
    (r1, r2)
}

/// Write a conflicting branch `2-bbbb` next to the existing one.
async fn add_conflict(db: &Database, id: &str, rev1: &str) {
    let hash1 = rev1.split_once('-').unwrap().1;
    let doc = Document::from_json(json!({
        "_id": id,
        "_rev": "2-bbbb",
        "_revisions": {"start": 2, "ids": ["bbbb", hash1]},
        "v": "other",
    }))
    .unwrap();
    let res = db
        .bulk_docs(vec![doc], BulkDocsOptions::replication())
        .await
        .unwrap();
    assert!(res.iter().all(|r| r.ok), "{res:?}");
}

// ─── _revs_diff ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn revs_diff_reports_missing_revisions() {
    let db = Arc::new(Database::memory(DB));
    let (_r1, r2) = doc_with_two_revs(&db).await;
    let app = app_with(db, &config());

    let resp = post(
        &app,
        "/db/_revs_diff",
        json!({"a": [r2.clone(), "3-zzz"], "nope": ["1-x"]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["a"]["missing"], json!(["3-zzz"]));
    assert_eq!(body["a"]["possible_ancestors"], json!([r2.clone()]));
    assert_eq!(body["nope"]["missing"], json!(["1-x"]));

    let resp = post(&app, "/db/_revs_diff", json!({"a": [r2]})).await;
    assert_eq!(resp.json(), json!({}));

    let resp = post(&app, "/db/_revs_diff", json!({"a": ["garbage"]})).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    let resp = post(&app, "/db/_revs_diff", json!([1])).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json(),
        bad_request("Request body must be a JSON object")
    );
}

// ─── _bulk_get ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn bulk_get_returns_requested_revisions() {
    let db = Arc::new(Database::memory(DB));
    let (r1, r2) = doc_with_two_revs(&db).await;
    let app = app_with(db, &config());

    let resp = post(
        &app,
        "/db/_bulk_get",
        json!({"docs": [
            {"id": "a"},
            {"id": "a", "rev": r1},
            {"id": "a", "rev": "9-nope"},
            {"id": "nope"},
        ]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let results = resp.json()["results"].clone();
    assert_eq!(results.as_array().unwrap().len(), 4);

    // `_revisions` only with revs=true.
    assert_eq!(
        results[0],
        json!({"id": "a", "docs": [{"ok": {"_id": "a", "_rev": r2, "v": 2}}]})
    );
    assert_eq!(
        results[1],
        json!({"id": "a", "docs": [{"ok": {"_id": "a", "_rev": r1, "v": 1}}]})
    );
    assert_eq!(
        results[2],
        json!({"id": "a", "docs": [{"error": {
            "id": "a", "rev": "9-nope", "error": "not_found", "reason": "missing",
        }}]})
    );
    let err = &results[3]["docs"][0]["error"];
    assert_eq!(results[3]["id"], "nope");
    assert_eq!(err["id"], "nope");
    assert_eq!(err["error"], "not_found");
    assert_eq!(err["reason"], "missing");

    let resp = post(
        &app,
        "/db/_bulk_get?revs=true",
        json!({"docs": [{"id": "a"}, {"id": "a", "rev": r1}]}),
    )
    .await;
    let results = resp.json()["results"].clone();
    assert_eq!(
        results[0]["docs"][0]["ok"]["_revisions"],
        json!({"start": 2, "ids": [hash(&r2), hash(&r1)]})
    );
    assert_eq!(
        results[1]["docs"][0]["ok"]["_revisions"],
        json!({"start": 1, "ids": [hash(&r1)]})
    );
}

#[tokio::test]
async fn bulk_get_validates_its_body() {
    let app = app();
    let resp = post(&app, "/db/_bulk_get", json!({})).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json(), bad_request("Missing JSON list of 'docs'."));

    let resp = post(&app, "/db/_bulk_get", json!({"docs": [{"rev": "1-a"}]})).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({"results": [{"id": null, "docs": [{"error": {
            "id": null, "rev": null, "error": "bad_request", "reason": "document id missed",
        }}]}]})
    );
}

// ─── _local ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn local_documents_crud() {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &config());

    let resp = put(&app, "/db/_local/ck", json!({"a": 1})).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(
        resp.json(),
        json!({"ok": true, "id": "_local/ck", "rev": "0-1"})
    );

    let resp = get(&app, "/db/_local/ck").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({"_id": "_local/ck", "_rev": "0-1", "a": 1})
    );

    let resp = put(&app, "/db/_local/ck", json!({"a": 2, "_rev": "0-1"})).await;
    assert_eq!(resp.json()["rev"], "0-2");
    assert_eq!(get(&app, "/db/_local/ck").await.json()["a"], 2);

    let resp = put(&app, "/db/_local/ck", json!({"_rev": "garbage"})).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json(), bad_request("Invalid rev format"));
    let resp = put(&app, "/db/_local/ck", json!([1])).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json(), bad_request("Document must be a JSON object"));

    // Local docs are not documents: not listed, not in the changes feed.
    assert_eq!(db.info().await.unwrap().doc_count, 0);
    let changes = get(&app, "/db/_changes").await.json();
    assert_eq!(changes["results"], json!([]));

    let resp = delete(&app, "/db/_local/ck").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({"ok": true, "id": "_local/ck", "rev": "0-0"})
    );
    let resp = get(&app, "/db/_local/ck").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    assert_eq!(resp.json(), missing());
    let resp = delete(&app, "/db/_local/ck").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    assert_eq!(resp.json(), missing());

    // Ids may contain a slash, encoded or not.
    let resp = put(&app, "/db/_local/a%2Fb", json!({"x": 1})).await;
    assert_eq!(resp.json()["id"], "_local/a/b");
    assert_eq!(get(&app, "/db/_local/a/b").await.json()["x"], 1);
}

// ─── _purge ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn purge_removes_revisions() {
    let db = Arc::new(Database::memory(DB));
    let rev = db.put("b", json!({})).await.unwrap().rev.unwrap();
    let app = app_with(db, &config());

    let resp = post(
        &app,
        "/db/_purge",
        json!({"b": [rev.clone()], "zz": ["1-a"]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(resp.json()["purged"], json!({"b": [rev], "zz": []}));
    assert_eq!(get(&app, "/db/b").await.status, StatusCode::NOT_FOUND);

    let resp = post(&app, "/db/_purge", json!([1])).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json(),
        bad_request("Request body must be a JSON object")
    );
    let resp = post(&app, "/db/_purge", json!({"c": "1-x"})).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json(), bad_request("Invalid list of revisions"));
}

// ─── open_revs ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn open_revs_returns_an_array_of_leaves() {
    let db = Arc::new(Database::memory(DB));
    let (r1, r2) = doc_with_two_revs(&db).await;
    add_conflict(&db, "a", &r1).await;
    let app = app_with(db, &config());

    let resp = get(&app, "/db/a?open_revs=all").await;
    assert_eq!(resp.status, StatusCode::OK);
    let leaves = resp.json();
    let mut revs: Vec<String> = leaves
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["ok"]["_rev"].as_str().unwrap().to_string())
        .collect();
    revs.sort();
    let mut expected = vec![r2.clone(), "2-bbbb".to_string()];
    expected.sort();
    assert_eq!(revs, expected);
    assert!(leaves[0]["ok"].get("_revisions").is_none());

    let resp = get(&app, "/db/a?open_revs=all&revs=true").await;
    let mut histories: Vec<(String, serde_json::Value)> = resp
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["ok"]["_rev"].as_str().unwrap().to_string(),
                l["ok"]["_revisions"].clone(),
            )
        })
        .collect();
    histories.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![
        (
            r2.clone(),
            json!({"start": 2, "ids": [hash(&r2), hash(&r1)]}),
        ),
        (
            "2-bbbb".to_string(),
            json!({"start": 2, "ids": ["bbbb", hash(&r1)]}),
        ),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(histories, expected);

    let uri = format!("/db/a?open_revs={}", q(json!([r1.clone(), "5-nope"])));
    let resp = get(&app, &uri).await;
    assert_eq!(
        resp.json(),
        json!([{"ok": {"_id": "a", "_rev": r1, "v": 1}}, {"missing": "5-nope"}])
    );

    let resp = get(&app, "/db/a?open_revs=bogus").await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json(), bad_request("invalid UTF-8 JSON"));
    let resp = get(&app, "/db/nope?open_revs=all").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    assert_eq!(resp.json(), missing());
    let uri = format!("/db/nope?open_revs={}", q(json!(["1-a"])));
    assert_eq!(get(&app, &uri).await.json(), json!([{"missing": "1-a"}]));
}

#[tokio::test]
async fn open_revs_all_includes_deleted_leaf() {
    let db = Arc::new(Database::memory(DB));
    let rev = db.put("gone", json!({"x": 1})).await.unwrap().rev.unwrap();
    let del = db.remove("gone", &rev).await.unwrap().rev.unwrap();
    let app = app_with(db, &config());

    let resp = get(&app, "/db/gone?open_revs=all").await;
    assert_eq!(resp.status, StatusCode::OK);
    let leaf = &resp.json()[0]["ok"];
    assert_eq!(leaf["_rev"], del.as_str());
    assert_eq!(leaf["_deleted"], true);
}

// ─── Real replication through Database::http ────────────────────────────────

async fn seed(db: &Database) {
    for i in 0..30 {
        db.put(&format!("doc{i:02}"), json!({"n": i}))
            .await
            .unwrap();
    }
    let r = db.get("doc00").await.unwrap().rev.unwrap().to_string();
    let r = db
        .update("doc00", &r, json!({"n": 100}))
        .await
        .unwrap()
        .rev
        .unwrap();
    db.update("doc00", &r, json!({"n": 200})).await.unwrap();
    let r = db.get("doc01").await.unwrap().rev.unwrap().to_string();
    db.remove("doc01", &r).await.unwrap();
}

async fn assert_same_docs(a: &Database, b: &Database) {
    let opts = || rouchdb::AllDocsOptions {
        include_docs: true,
        ..rouchdb::AllDocsOptions::new()
    };
    let rows_a = a.all_docs(opts()).await.unwrap().rows;
    let rows_b = b.all_docs(opts()).await.unwrap().rows;
    assert_eq!(rows_a.len(), 29);
    let summary = |rows: &[rouchdb::AllDocsRow]| -> Vec<(String, String, serde_json::Value)> {
        rows.iter()
            .map(|r| {
                (
                    r.id.clone(),
                    r.value.rev.clone(),
                    r.doc.clone().unwrap()["n"].clone(),
                )
            })
            .collect()
    };
    assert_eq!(summary(&rows_a), summary(&rows_b));
    assert!(b.get("doc01").await.is_err(), "deletion replicated");
}

fn small_batches() -> ReplicationOptions {
    ReplicationOptions {
        batch_size: 7,
        ..Default::default()
    }
}

#[tokio::test]
async fn replicate_memory_to_server() {
    let server_db = Arc::new(Database::memory(DB));
    let addr = serve(app_with(server_db.clone(), &config())).await;
    let remote = Database::http(&format!("http://{addr}/{DB}"));

    let local = Database::memory("local");
    seed(&local).await;

    let result = local
        .replicate_to_with_opts(&remote, small_batches())
        .await
        .unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(result.docs_written, 30);
    assert_same_docs(&local, &server_db).await;

    // The checkpoint lives in `_local` on the server, so a second run
    // resumes from it instead of re-reading everything.
    let again = local
        .replicate_to_with_opts(&remote, small_batches())
        .await
        .unwrap();
    assert!(again.ok);
    assert_eq!(again.docs_read, 0);
}

#[tokio::test]
async fn replicate_server_to_memory() {
    let server_db = Arc::new(Database::memory(DB));
    seed(&server_db).await;
    let addr = serve(app_with(server_db.clone(), &config())).await;
    let remote = Database::http(&format!("http://{addr}/{DB}"));

    let local = Database::memory("local");
    let result = remote
        .replicate_to_with_opts(&local, small_batches())
        .await
        .unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_same_docs(&server_db, &local).await;

    let again = remote
        .replicate_to_with_opts(&local, small_batches())
        .await
        .unwrap();
    assert_eq!(again.docs_read, 0);
}

#[tokio::test]
async fn replicate_memory_to_redb_backed_server() {
    let dir = tempfile::tempdir().unwrap();
    let server_db = Arc::new(Database::open(dir.path().join("db.redb"), DB).unwrap());
    let addr = serve(app_with(server_db.clone(), &config())).await;
    let remote = Database::http(&format!("http://{addr}/{DB}"));

    let local = Database::memory("local");
    seed(&local).await;
    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_same_docs(&local, &server_db).await;
    assert_eq!(local.replicate_to(&remote).await.unwrap().docs_read, 0);
}

// ─── Q-SRV-5: full revision state survives replication both ways ────────────

/// Non-UTF-8 attachment bytes, distinct per prefix so an attachment can
/// never be satisfied by identical bytes already stored for another doc.
fn blob(prefix: &str) -> Vec<u8> {
    [&[0, 1, 2, 255, 254][..], prefix.as_bytes()].concat()
}

/// Write, under `prefix`, a document with a 5-revision history, one with
/// two live branches plus a deleted one, one with binary and text
/// attachments, and a tombstone.
async fn seed_revision_shapes(db: &Database, prefix: &str) {
    let id = format!("{prefix}hist");
    let mut rev = db.put(&id, json!({"v": 1})).await.unwrap().rev.unwrap();
    for v in 2..=5 {
        rev = db
            .update(&id, &rev, json!({"v": v}))
            .await
            .unwrap()
            .rev
            .unwrap();
    }

    // conf: 1-x -> 2-y (local edit), 1-x -> 2-bbbb -> 3-cccc (replicated
    // branch, the winner) and 1-x -> 2-dddd (deleted).
    let id = format!("{prefix}conf");
    let r1 = db
        .put(&id, json!({"side": "a"}))
        .await
        .unwrap()
        .rev
        .unwrap();
    db.update(&id, &r1, json!({"side": "a2"})).await.unwrap();
    let h1 = r1.split_once('-').unwrap().1;
    let branches = vec![
        Document::from_json(json!({
            "_id": id,
            "_rev": "3-cccc",
            "_revisions": {"start": 3, "ids": ["cccc", "bbbb", h1]},
            "side": "b",
        }))
        .unwrap(),
        Document::from_json(json!({
            "_id": id,
            "_rev": "2-dddd",
            "_deleted": true,
            "_revisions": {"start": 2, "ids": ["dddd", h1]},
        }))
        .unwrap(),
    ];
    let res = db
        .bulk_docs(branches, BulkDocsOptions::replication())
        .await
        .unwrap();
    assert!(res.iter().all(|r| r.ok), "{res:?}");

    let id = format!("{prefix}att");
    let rev = db
        .put(&id, json!({"kind": "files"}))
        .await
        .unwrap()
        .rev
        .unwrap();
    let rev = db
        .put_attachment(
            &id,
            "blob.bin",
            &rev,
            blob(prefix),
            "application/octet-stream",
        )
        .await
        .unwrap()
        .rev
        .unwrap();
    let text = format!("hello from {prefix}").into_bytes();
    db.put_attachment(&id, "note.txt", &rev, text, "text/plain")
        .await
        .unwrap();

    let id = format!("{prefix}gone");
    let rev = db.put(&id, json!({"x": 1})).await.unwrap().rev.unwrap();
    db.remove(&id, &rev).await.unwrap();
}

/// One leaf revision: body, deleted flag, ancestry and attachments with
/// their bytes.
#[derive(Debug, PartialEq)]
struct Leaf {
    rev: String,
    deleted: bool,
    body: serde_json::Value,
    revisions: serde_json::Value,
    /// name -> (content_type, digest, length, bytes)
    attachments: std::collections::BTreeMap<String, (String, String, u64, Vec<u8>)>,
}

#[derive(Debug, PartialEq)]
struct DocState {
    id: String,
    winner: String,
    deleted: bool,
    conflicts: Vec<String>,
    leaves: Vec<Leaf>,
}

/// Every document (tombstones included) with its winner, conflicts and
/// every leaf revision in full.
async fn full_snapshot(db: &Database) -> Vec<DocState> {
    use rouchdb::{ChangesOptions, ChangesStyle, GetOptions};

    let winners = db.changes(ChangesOptions::default()).await.unwrap().results;
    let all_leaves = db
        .changes(ChangesOptions {
            style: ChangesStyle::AllDocs,
            ..Default::default()
        })
        .await
        .unwrap()
        .results;

    let mut docs = Vec::new();
    for change in winners {
        let winner = change.changes[0].rev.clone();
        let doc = db
            .get_with_opts(
                &change.id,
                GetOptions {
                    rev: Some(winner.clone()),
                    conflicts: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut conflicts: Vec<String> = doc.data["_conflicts"]
            .as_array()
            .map(|a| a.iter().map(|r| r.as_str().unwrap().to_string()).collect())
            .unwrap_or_default();
        conflicts.sort();

        let mut revs: Vec<String> = all_leaves
            .iter()
            .find(|c| c.id == change.id)
            .unwrap()
            .changes
            .iter()
            .map(|c| c.rev.clone())
            .collect();
        revs.sort();
        let mut leaves = Vec::new();
        for rev in revs {
            let mut leaf = db
                .get_with_opts(
                    &change.id,
                    GetOptions {
                        rev: Some(rev.clone()),
                        revs: true,
                        attachments: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let revisions = leaf.data.as_object_mut().unwrap().remove("_revisions");
            leaves.push(Leaf {
                rev,
                deleted: leaf.deleted,
                body: leaf.data,
                revisions: revisions.unwrap_or_default(),
                attachments: leaf
                    .attachments
                    .into_iter()
                    .map(|(name, meta)| {
                        let data = meta.data.unwrap_or_default();
                        (name, (meta.content_type, meta.digest, meta.length, data))
                    })
                    .collect(),
            });
        }
        docs.push(DocState {
            id: change.id,
            winner,
            deleted: change.deleted,
            conflicts,
            leaves,
        });
    }
    docs.sort_by(|a, b| a.id.cmp(&b.id));
    docs
}

/// Guard against a vacuous comparison: the snapshot really holds every
/// shape seeded under `prefix`.
fn assert_has_shapes(snapshot: &[DocState], prefix: &str) {
    let doc = |name: &str| {
        let id = format!("{prefix}{name}");
        snapshot
            .iter()
            .find(|d| d.id == id)
            .unwrap_or_else(|| panic!("{id} missing from {snapshot:#?}"))
    };

    let hist = doc("hist");
    assert!(hist.winner.starts_with("5-"), "{hist:?}");
    assert_eq!(hist.leaves[0].revisions["start"], 5);
    assert_eq!(hist.leaves[0].revisions["ids"].as_array().unwrap().len(), 5);
    assert_eq!(hist.leaves[0].body, json!({"v": 5}));

    let conf = doc("conf");
    assert_eq!(conf.winner, "3-cccc");
    assert_eq!(conf.conflicts.len(), 1, "{conf:?}");
    assert!(conf.conflicts[0].starts_with("2-"), "{conf:?}");
    assert_eq!(conf.leaves.len(), 3, "{conf:?}");
    let deleted: Vec<_> = conf.leaves.iter().filter(|l| l.deleted).collect();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].rev, "2-dddd");
    let winner = conf.leaves.iter().find(|l| l.rev == "3-cccc").unwrap();
    assert_eq!(winner.revisions["start"], 3);
    assert_eq!(
        winner.revisions["ids"].as_array().unwrap()[..2],
        [json!("cccc"), json!("bbbb")]
    );

    let att = doc("att");
    let atts = &att.leaves[0].attachments;
    let bytes = blob(prefix);
    assert_eq!(
        atts["blob.bin"],
        (
            "application/octet-stream".to_string(),
            rouchdb::attachment_digest(&bytes),
            bytes.len() as u64,
            bytes,
        )
    );
    assert_eq!(atts["note.txt"].0, "text/plain");
    assert_eq!(
        atts["note.txt"].3,
        format!("hello from {prefix}").as_bytes()
    );

    let gone = doc("gone");
    assert!(gone.deleted && gone.winner.starts_with("2-"), "{gone:?}");
    assert!(gone.leaves[0].deleted);
}

/// The replication checkpoint the server stores for `rep_id` matches the
/// replication's result and the checkpoint on the other side.
async fn assert_checkpoint(
    app: &axum::Router,
    local: &Database,
    rep_id: &str,
    last_seq: &rouchdb::Seq,
) {
    let resp = get(app, &format!("/db/_local/{rep_id}")).await;
    assert_eq!(resp.status, StatusCode::OK, "{rep_id}");
    let on_server = resp.json();
    assert_eq!(on_server["_id"], format!("_local/{rep_id}"));
    assert_eq!(on_server["last_seq"], json!(last_seq));

    let on_local = local.adapter().get_local(rep_id).await.unwrap();
    assert_eq!(on_local["last_seq"], json!(last_seq));
    let session = on_local["session_id"].as_str().unwrap();
    assert!(!session.is_empty());
    assert_eq!(on_server["session_id"], session);
    assert_eq!(on_server["history"][0]["session_id"], session);
    assert_eq!(on_server["history"][0]["last_seq"], json!(last_seq));
}

async fn replication_id(source: &Database, target: &Database) -> String {
    rouchdb_replication::Checkpointer::new(
        &source.adapter().id().await.unwrap(),
        &target.adapter().id().await.unwrap(),
        "nofilter",
    )
    .replication_id()
    .to_string()
}

#[tokio::test]
async fn replication_through_the_server_keeps_full_revision_state() {
    let server_db = Arc::new(Database::memory(DB));
    seed_revision_shapes(&server_db, "s-").await;
    let app = app_with(server_db.clone(), &config());
    let addr = serve(app.clone()).await;
    let remote = Database::http(&format!("http://{addr}/{DB}"));

    let local = Database::memory("local");
    seed_revision_shapes(&local, "l-").await;

    // Push: the server's _bulk_docs must keep `_revisions`, deleted leaves
    // and inline attachments.
    let pushed = local.replicate_to(&remote).await.unwrap();
    assert!(pushed.ok, "{:?}", pushed.errors);
    // One write per leaf: hist, att and gone have one, conf three.
    assert_eq!(pushed.docs_written, 6);
    let push_id = replication_id(&local, &remote).await;
    assert_checkpoint(&app, &local, &push_id, &pushed.last_seq).await;

    // Pull: the server's _bulk_get must return every leaf with its history
    // and attachment bytes.
    let pulled = local.replicate_from(&remote).await.unwrap();
    assert!(pulled.ok, "{:?}", pulled.errors);
    assert_eq!(pulled.docs_written, 6);
    let pull_id = replication_id(&remote, &local).await;
    assert_ne!(pull_id, push_id);
    assert_checkpoint(&app, &local, &pull_id, &pulled.last_seq).await;

    let on_server = full_snapshot(&server_db).await;
    let on_local = full_snapshot(&local).await;
    for prefix in ["s-", "l-"] {
        assert_has_shapes(&on_server, prefix);
    }
    assert_eq!(on_local, on_server);

    // Nothing left to move in either direction.
    assert_eq!(local.replicate_to(&remote).await.unwrap().docs_written, 0);
    assert_eq!(local.replicate_from(&remote).await.unwrap().docs_written, 0);
    assert_eq!(full_snapshot(&server_db).await, on_server);
}
