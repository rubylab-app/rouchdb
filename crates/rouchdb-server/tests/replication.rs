//! F19: the endpoints the CouchDB replication protocol needs, and real
//! replication through `Database::http` in both directions.
mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use rouchdb::{BulkDocsOptions, Database, Document, ReplicationOptions};
use serde_json::json;

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
    assert_eq!(resp.json()["error"], "bad_request");
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

    let latest = &results[0]["docs"][0]["ok"];
    assert_eq!(latest["_id"], "a");
    assert_eq!(latest["_rev"], r2.as_str());
    assert_eq!(latest["v"], 2);
    assert!(latest.get("_revisions").is_none(), "only with revs=true");

    assert_eq!(results[1]["docs"][0]["ok"]["_rev"], r1.as_str());
    assert_eq!(results[1]["docs"][0]["ok"]["v"], 1);

    let err = &results[2]["docs"][0]["error"];
    assert_eq!(err["id"], "a");
    assert_eq!(err["rev"], "9-nope");
    assert_eq!(err["error"], "not_found");
    assert_eq!(results[3]["docs"][0]["error"]["error"], "not_found");

    let resp = post(
        &app,
        "/db/_bulk_get?revs=true",
        json!({"docs": [{"id": "a"}]}),
    )
    .await;
    let revisions = resp.json()["results"][0]["docs"][0]["ok"]["_revisions"].clone();
    assert_eq!(revisions["start"], 2);
    assert_eq!(revisions["ids"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn bulk_get_validates_its_body() {
    let app = app();
    let resp = post(&app, "/db/_bulk_get", json!({})).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json()["error"], "bad_request");

    let resp = post(&app, "/db/_bulk_get", json!({"docs": [{"rev": "1-a"}]})).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json()["results"][0]["docs"][0]["error"]["error"],
        "bad_request"
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
    let resp = put(&app, "/db/_local/ck", json!([1])).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);

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
    assert_eq!(
        get(&app, "/db/_local/ck").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete(&app, "/db/_local/ck").await.status,
        StatusCode::NOT_FOUND
    );

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
    let body = resp.json();
    assert_eq!(body["purged"]["b"], json!([rev]));
    assert_eq!(body["purged"]["zz"], json!([]));
    assert_eq!(get(&app, "/db/b").await.status, StatusCode::NOT_FOUND);

    let resp = post(&app, "/db/_purge", json!([1])).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    let resp = post(&app, "/db/_purge", json!({"c": "1-x"})).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
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
    assert!(resp.json()[0]["ok"]["_revisions"]["ids"].is_array());

    let uri = format!("/db/a?open_revs={}", q(json!([r1.clone(), "5-nope"])));
    let resp = get(&app, &uri).await;
    assert_eq!(
        resp.json(),
        json!([{"ok": {"_id": "a", "_rev": r1, "v": 1}}, {"missing": "5-nope"}])
    );

    assert_eq!(
        get(&app, "/db/a?open_revs=bogus").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&app, "/db/nope?open_revs=all").await.status,
        StatusCode::NOT_FOUND
    );
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
