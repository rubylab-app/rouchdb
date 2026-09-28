//! `DELETE /{db}` removes the database until `PUT /{db}` creates it again,
//! as in CouchDB 3.5.1 (404 "Database does not exist." in between).
mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use rouchdb::Database;
use serde_json::json;

#[track_caller]
fn assert_error(resp: &Resp, status: StatusCode, error: &str, reason: &str) {
    assert_eq!(
        resp.status,
        status,
        "{:?}",
        String::from_utf8_lossy(&resp.body)
    );
    let body = resp.json();
    assert_eq!(body["error"], error, "{body}");
    assert_eq!(body["reason"], reason, "{body}");
}

#[tokio::test]
async fn deleted_database_is_gone_until_recreated() {
    let db = Arc::new(Database::memory(DB));
    db.put("doc", json!({"v": 1})).await.unwrap();
    let app = app_with(db.clone(), &config());

    let resp = delete(&app, "/db").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"ok": true}));

    let gone = |resp: &Resp| {
        assert_error(
            resp,
            StatusCode::NOT_FOUND,
            "not_found",
            "Database does not exist.",
        )
    };
    gone(&get(&app, "/db").await);
    gone(&get(&app, "/db/doc").await);
    gone(&put(&app, "/db/doc", json!({"v": 2})).await);
    gone(&post(&app, "/db", json!({"v": 2})).await);
    gone(&get(&app, "/db/_all_docs").await);
    gone(&get(&app, "/db/_changes").await);
    gone(&post(&app, "/db/_find", json!({"selector": {}})).await);
    gone(&post(&app, "/db/_bulk_docs", json!({"docs": [{"_id": "x"}]})).await);
    gone(&put(&app, "/db/_local/l", json!({})).await);
    gone(&get(&app, "/db/_security").await);
    gone(&delete(&app, "/db").await);
    assert_eq!(get(&app, "/_all_dbs").await.json(), json!([]));
    // Nothing was written behind the 404s.
    assert_eq!(db.info().await.unwrap().doc_count, 0);

    // PUT /{db} creates it again, empty.
    let resp = call(&app, Method::PUT, "/db", None).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(resp.json(), json!({"ok": true}));
    let info = get(&app, "/db").await;
    assert_eq!(info.status, StatusCode::OK);
    assert_eq!(info.json()["doc_count"], 0);
    assert_eq!(get(&app, "/_all_dbs").await.json(), json!([DB]));
    assert_eq!(
        put(&app, "/db/doc", json!({"v": 3})).await.status,
        StatusCode::CREATED
    );

    // Creating an existing database is a 412 with CouchDB's reason.
    assert_error(
        &call(&app, Method::PUT, "/db", None).await,
        StatusCode::PRECONDITION_FAILED,
        "file_exists",
        "The database could not be created, the file already exists.",
    );
}

#[tokio::test]
async fn deleted_database_ends_waiting_changes_feeds() {
    let app = app();
    let feed = {
        let app = app.clone();
        tokio::spawn(async move { get(&app, "/db/_changes?feed=longpoll&since=now").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(delete(&app, "/db").await.status, StatusCode::OK);
    let resp = tokio::time::timeout(Duration::from_secs(5), feed)
        .await
        .expect("the longpoll feed must end when the database is deleted")
        .unwrap();
    assert_eq!(resp.status, StatusCode::OK);
}
