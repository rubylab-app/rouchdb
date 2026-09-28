//! F49: `_find` pages with a real bookmark and defaults to limit 25.
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
