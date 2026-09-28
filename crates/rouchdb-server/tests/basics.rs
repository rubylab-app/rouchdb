//! F72: smoke coverage of the remaining CouchDB endpoints.
mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn server_level_endpoints() {
    let app = app();
    let root = get(&app, "/").await.json();
    assert_eq!(root["couchdb"], "Welcome");
    assert_eq!(get(&app, "/_all_dbs").await.json(), json!([DB]));
    let uuids = get(&app, "/_uuids?count=3").await.json();
    assert_eq!(uuids["uuids"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn database_endpoints() {
    let app = app();
    let info = get(&app, "/db").await;
    assert_eq!(info.status, StatusCode::OK);
    assert_eq!(info.json()["doc_count"], 0);

    assert_eq!(get(&app, "/other").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&app, axum::http::Method::PUT, "/db", None)
            .await
            .status,
        StatusCode::PRECONDITION_FAILED
    );
    let resp = post(&app, "/db/_compact", json!({})).await;
    assert_eq!(resp.status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn document_crud() {
    let app = app();
    let resp = post(&app, "/db", json!({"a": 1})).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let id = resp.json()["id"].as_str().unwrap().to_string();

    let resp = put(&app, "/db/doc", json!({"v": 1})).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let rev = resp.json()["rev"].as_str().unwrap().to_string();
    assert_eq!(
        put(&app, "/db/doc", json!({"v": 2})).await.status,
        StatusCode::CONFLICT
    );

    let resp = put(&app, "/db/doc", json!({"_rev": rev, "v": 2})).await;
    let rev2 = resp.json()["rev"].as_str().unwrap().to_string();
    assert_eq!(get(&app, "/db/doc").await.json()["v"], 2);

    let resp = delete(&app, &format!("/db/doc?rev={rev2}")).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(get(&app, "/db/doc").await.status, StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/db").await.json()["doc_count"], 1);
    assert_eq!(get(&app, &format!("/db/{id}")).await.json()["a"], 1);
}

#[tokio::test]
async fn bulk_docs_both_modes() {
    let app = app();
    let resp = post(
        &app,
        "/db/_bulk_docs",
        json!({"docs": [{"_id": "x"}, {"_id": "y"}]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(resp.json().as_array().unwrap().len(), 2);

    let resp = post(
        &app,
        "/db/_bulk_docs",
        json!({"new_edits": false, "docs": [{"_id": "z", "_rev": "3-abc", "k": 1}]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(get(&app, "/db/z").await.json()["_rev"], "3-abc");
}

#[tokio::test]
async fn security_document_round_trip() {
    let app = app();
    let sec =
        json!({"admins": {"names": ["a"], "roles": []}, "members": {"names": [], "roles": ["r"]}});
    assert_eq!(put(&app, "/db/_security", sec).await.status, StatusCode::OK);
    let got = get(&app, "/db/_security").await.json();
    assert_eq!(got["admins"]["names"], json!(["a"]));
    assert_eq!(got["members"]["roles"], json!(["r"]));
}

#[tokio::test]
async fn javascript_views_are_reported_as_unsupported() {
    let app = app();
    put(
        &app,
        "/db/_design/d",
        json!({"views": {"v": {"map": "function(doc){}"}}}),
    )
    .await;
    let resp = get(&app, "/db/_design/d/_view/v").await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json()["error"], "bad_request");
    assert_eq!(
        get(&app, "/db/_design/d/_view/none").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(&app, "/db/_design/d/_info").await.json()["name"], "d");
}
