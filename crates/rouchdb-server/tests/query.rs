//! F49: `_find` pages with a real bookmark and defaults to limit 25.
//! F63: Mango indexes are persisted as `language: "query"` design documents.
mod common;

use std::collections::HashSet;
use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use rouchdb::Database;
use serde_json::json;

async fn thirty_docs() -> (Arc<Database>, axum::Router) {
    let db = Arc::new(Database::memory(DB));
    for i in 0..30 {
        db.put(&format!("d{i:02}"), json!({"n": i})).await.unwrap();
    }
    let app = app_with(db.clone(), &config());
    (db, app)
}

fn doc_ids(resp: &Resp) -> Vec<String> {
    resp.json()["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn find_defaults_to_25_results() {
    let (_db, app) = thirty_docs().await;
    let resp = post(&app, "/db/_find", json!({"selector": {"n": {"$gte": 0}}})).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(doc_ids(&resp).len(), 25);

    let resp = post(
        &app,
        "/db/_find",
        json!({"selector": {"n": {"$gte": 0}}, "limit": 100}),
    )
    .await;
    assert_eq!(doc_ids(&resp).len(), 30);
}

#[tokio::test]
async fn find_bookmark_pages_through_all_results() {
    let (_db, app) = thirty_docs().await;
    let mut seen = HashSet::new();
    let mut bookmark: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut body = json!({"selector": {"n": {"$gte": 0}}, "limit": 7});
        if let Some(b) = &bookmark {
            body["bookmark"] = json!(b);
        }
        let resp = post(&app, "/db/_find", body).await;
        assert_eq!(resp.status, StatusCode::OK);
        let ids = doc_ids(&resp);
        let next = resp.json()["bookmark"].as_str().unwrap().to_string();
        if ids.is_empty() {
            break;
        }
        for id in ids {
            assert!(seen.insert(id.clone()), "{id} returned twice");
        }
        assert_ne!(next, "nil");
        bookmark = Some(next);
        pages += 1;
        assert!(pages < 10, "pagination does not terminate");
    }
    assert_eq!(seen.len(), 30);
    assert_eq!(pages, 5);
}

#[tokio::test]
async fn find_bookmark_combines_with_skip() {
    let (_db, app) = thirty_docs().await;
    let selector = json!({"n": {"$gte": 0}});
    let sort = json!([{"n": "asc"}]);
    let first = post(
        &app,
        "/db/_find",
        json!({"selector": selector, "sort": sort, "limit": 3}),
    )
    .await;
    assert_eq!(doc_ids(&first), ["d00", "d01", "d02"]);
    let bookmark = first.json()["bookmark"].clone();

    let resp = post(
        &app,
        "/db/_find",
        json!({"selector": selector, "sort": sort, "limit": 2, "skip": 2, "bookmark": bookmark}),
    )
    .await;
    assert_eq!(doc_ids(&resp), ["d05", "d06"]);
}

#[tokio::test]
async fn find_rejects_invalid_bookmarks() {
    let (_db, app) = thirty_docs().await;
    for bookmark in ["garbage", "nil"] {
        let resp = post(
            &app,
            "/db/_find",
            json!({"selector": {}, "bookmark": bookmark}),
        )
        .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{bookmark}");
        assert_eq!(resp.json()["error"], "invalid_bookmark");
    }
}

#[tokio::test]
async fn find_without_matches_returns_nil_bookmark() {
    let (_db, app) = thirty_docs().await;
    let resp = post(&app, "/db/_find", json!({"selector": {"n": {"$gt": 1000}}})).await;
    assert_eq!(resp.json(), json!({"docs": [], "bookmark": "nil"}));
}

// ─── F63: persisted indexes ─────────────────────────────────────────────────

fn index_names(resp: &Resp) -> Vec<(serde_json::Value, String)> {
    resp.json()["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["ddoc"].clone(), i["name"].as_str().unwrap().to_string()))
        .collect()
}

#[tokio::test]
async fn create_index_writes_a_query_design_doc() {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &config());

    let body = json!({"index": {"fields": ["n"]}, "name": "by-n", "ddoc": "my-idx"});
    let resp = post(&app, "/db/_index", body.clone()).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({"result": "created", "id": "_design/my-idx", "name": "by-n"})
    );
    let resp = post(&app, "/db/_index", body).await;
    assert_eq!(
        resp.json(),
        json!({"result": "exists", "id": "_design/my-idx", "name": "by-n"})
    );
    let resp = post(
        &app,
        "/db/_index",
        json!({"index": {"fields": [{"t": "desc"}]}, "name": "by-t", "ddoc": "_design/my-idx"}),
    )
    .await;
    assert_eq!(resp.json()["result"], "created");

    let ddoc = db.get("_design/my-idx").await.unwrap().to_json();
    assert_eq!(ddoc["language"], "query");
    assert_eq!(ddoc["views"]["by-n"]["map"]["fields"], json!({"n": "asc"}));
    assert_eq!(
        ddoc["views"]["by-n"]["options"]["def"]["fields"],
        json!(["n"])
    );
    assert_eq!(ddoc["views"]["by-t"]["map"]["fields"], json!({"t": "desc"}));

    let resp = get(&app, "/db/_index").await;
    assert_eq!(
        index_names(&resp),
        [
            (json!(null), "_all_docs".to_string()),
            (json!("_design/my-idx"), "by-n".to_string()),
            (json!("_design/my-idx"), "by-t".to_string()),
        ]
    );
}

#[tokio::test]
async fn unnamed_index_gets_a_design_doc() {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &config());
    let resp = post(&app, "/db/_index", json!({"index": {"fields": ["n"]}})).await;
    let body = resp.json();
    assert_eq!(body["result"], "created");
    let id = body["id"].as_str().unwrap();
    assert!(id.starts_with("_design/"), "{body}");
    let name = body["name"].as_str().unwrap();
    assert!(db.get(id).await.unwrap().data["views"][name].is_object());
}

#[tokio::test]
async fn indexes_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.redb");
    {
        let db = Arc::new(Database::open(&path, DB).unwrap());
        let app = app_with(db, &config());
        let resp = post(
            &app,
            "/db/_index",
            json!({"index": {"fields": ["n"]}, "name": "by-n", "ddoc": "my-idx"}),
        )
        .await;
        assert_eq!(resp.json()["result"], "created");
    }

    let db = Arc::new(Database::open(&path, DB).unwrap());
    rouchdb_server::restore_indexes(&db).await.unwrap();
    let names: Vec<String> = db.get_indexes().await.into_iter().map(|i| i.name).collect();
    assert_eq!(names, ["by-n"]);

    let app = app_with(db, &config());
    let resp = get(&app, "/db/_index").await;
    assert!(index_names(&resp).contains(&(json!("_design/my-idx"), "by-n".to_string())));
}

#[tokio::test]
async fn delete_index_requires_matching_ddoc_and_type() {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &config());
    for (name, field) in [("by-n", "n"), ("by-t", "t")] {
        let body = json!({"index": {"fields": [field]}, "name": name, "ddoc": "my-idx"});
        assert_eq!(
            post(&app, "/db/_index", body).await.json()["result"],
            "created"
        );
    }

    for uri in [
        "/db/_index/other/json/by-n",
        "/db/_index/my-idx/text/by-n",
        "/db/_index/my-idx/json/nope",
    ] {
        let resp = delete(&app, uri).await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            resp.json(),
            json!({"error": "not_found", "reason": "missing"}),
            "{uri}"
        );
    }
    assert_eq!(db.get_indexes().await.len(), 2);

    let resp = delete(&app, "/db/_index/my-idx/json/by-n").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"ok": true}));
    let ddoc = db.get("_design/my-idx").await.unwrap().to_json();
    assert!(ddoc["views"].get("by-n").is_none());
    assert!(ddoc["views"]["by-t"].is_object());

    // The last index of a design document removes the document.
    let resp = delete(&app, "/db/_index/_design/my-idx/json/by-t").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(db.get("_design/my-idx").await.is_err());
    assert!(db.get_indexes().await.is_empty());
}

#[tokio::test]
async fn bulk_delete_removes_index_design_docs() {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &config());
    let body = json!({"index": {"fields": ["n"]}, "name": "by-n", "ddoc": "my-idx"});
    post(&app, "/db/_index", body).await;

    let resp = post(
        &app,
        "/db/_index/_bulk_delete",
        json!({"docids": ["_design/my-idx", "_design/nope"]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({
            "success": [{"id": "_design/my-idx", "ok": true}],
            "fail": [{"id": "_design/nope", "error": "not_found"}],
        })
    );
    assert!(db.get("_design/my-idx").await.is_err());
    assert!(db.get_indexes().await.is_empty());
}
