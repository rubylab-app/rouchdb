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
    let info = info.json();
    assert_eq!(info["db_name"], DB);
    assert_eq!(info["doc_count"], 0);
    assert_eq!(info["doc_del_count"], 0);

    let resp = get(&app, "/other").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    assert_eq!(resp.json()["error"], "not_found");
    let resp = call(&app, axum::http::Method::PUT, "/db", None).await;
    assert_eq!(resp.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(resp.json()["error"], "file_exists");
    let resp = post(&app, "/db/_compact", json!({})).await;
    assert_eq!(resp.status, StatusCode::ACCEPTED);
    assert_eq!(resp.json(), json!({"ok": true}));
}

fn rev_with_prefix(resp: &Resp, prefix: &str) -> String {
    let rev = resp.json()["rev"].as_str().unwrap().to_string();
    assert!(rev.starts_with(prefix), "{rev} should start with {prefix}");
    rev
}

#[tokio::test]
async fn document_crud() {
    let app = app();
    let resp = post(&app, "/db", json!({"a": 1})).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let id = resp.json()["id"].as_str().unwrap().to_string();
    // Like CouchDB's generated ids: 32 lowercase hex digits.
    assert_eq!(id.len(), 32, "{id}");
    assert!(
        id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{id}"
    );
    let rev = rev_with_prefix(&resp, "1-");
    assert_eq!(resp.json(), json!({"ok": true, "id": id, "rev": rev}));
    assert_eq!(
        get(&app, &format!("/db/{id}")).await.json(),
        json!({"_id": id, "_rev": rev, "a": 1})
    );

    let resp = put(&app, "/db/doc", json!({"v": 1})).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let rev = rev_with_prefix(&resp, "1-");
    assert_eq!(resp.json(), json!({"ok": true, "id": "doc", "rev": rev}));
    let resp = put(&app, "/db/doc", json!({"v": 2})).await;
    assert_eq!(resp.status, StatusCode::CONFLICT);
    assert_eq!(resp.json()["error"], "conflict");

    let resp = put(&app, "/db/doc", json!({"_rev": rev, "v": 2})).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let rev2 = rev_with_prefix(&resp, "2-");
    assert_eq!(resp.json(), json!({"ok": true, "id": "doc", "rev": rev2}));
    assert_eq!(
        get(&app, "/db/doc").await.json(),
        json!({"_id": "doc", "_rev": rev2, "v": 2})
    );

    let resp = delete(&app, &format!("/db/doc?rev={rev2}")).await;
    assert_eq!(resp.status, StatusCode::OK);
    let rev3 = rev_with_prefix(&resp, "3-");
    assert_eq!(resp.json(), json!({"ok": true, "id": "doc", "rev": rev3}));
    assert_eq!(get(&app, "/db/doc").await.status, StatusCode::NOT_FOUND);
    let info = get(&app, "/db").await.json();
    assert_eq!(info["doc_count"], 1);
    assert_eq!(info["doc_del_count"], 1);
}

#[tokio::test]
async fn bulk_docs_both_modes() {
    let app = app();
    let resp = post(
        &app,
        "/db/_bulk_docs",
        json!({"docs": [{"_id": "x"}, {"_id": "y", "v": 1}]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let stored_rev = |doc: serde_json::Value| doc["_rev"].as_str().unwrap().to_string();
    let x_rev = stored_rev(get(&app, "/db/x").await.json());
    let y_rev = stored_rev(get(&app, "/db/y").await.json());
    assert!(x_rev.starts_with("1-") && y_rev.starts_with("1-"));
    assert_eq!(
        resp.json(),
        json!([
            {"ok": true, "id": "x", "rev": x_rev},
            {"ok": true, "id": "y", "rev": y_rev},
        ])
    );

    let resp = post(
        &app,
        "/db/_bulk_docs",
        json!({"new_edits": false, "docs": [{"_id": "z", "_rev": "3-abc", "k": 1}]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(
        get(&app, "/db/z").await.json(),
        json!({"_id": "z", "_rev": "3-abc", "k": 1})
    );
}

#[tokio::test]
async fn security_document_round_trip() {
    let app = app();
    let sec =
        json!({"admins": {"names": ["a"], "roles": []}, "members": {"names": [], "roles": ["r"]}});
    let resp = put(&app, "/db/_security", sec.clone()).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"ok": true}));
    assert_eq!(get(&app, "/db/_security").await.json(), sec);
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
