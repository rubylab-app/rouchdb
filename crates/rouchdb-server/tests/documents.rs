//! F109: ETag / If-None-Match / If-Match and coherent revision sources.
mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::*;
use rouchdb::Database;
use serde_json::json;

async fn with_doc() -> (Arc<Database>, axum::Router, String) {
    let db = Arc::new(Database::memory(DB));
    let rev = db.put("a", json!({"v": 1})).await.unwrap().rev.unwrap();
    let app = app_with(db.clone(), &config());
    (db, app, rev)
}

fn req(
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut r = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    match body {
        Some(b) => r
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => r.body(Body::empty()).unwrap(),
    }
}

#[tokio::test]
async fn get_sends_etag_and_honors_if_none_match() {
    let (_db, app, rev) = with_doc().await;
    let etag = format!("\"{rev}\"");

    let resp = get(&app, "/db/a").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.header("etag"), Some(etag.as_str()));

    let resp = send(
        &app,
        req(Method::GET, "/db/a", &[("if-none-match", &etag)], None),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_MODIFIED);
    assert!(resp.body.is_empty());
    assert_eq!(resp.header("etag"), Some(etag.as_str()));

    let resp = send(
        &app,
        req(
            Method::GET,
            "/db/a",
            &[("if-none-match", "\"1-other\"")],
            None,
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
}

#[tokio::test]
async fn query_and_body_revs_must_agree() {
    let (db, app, rev) = with_doc().await;
    let resp = put(
        &app,
        &format!("/db/a?rev={rev}"),
        json!({"_rev": "1-other", "v": 2}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json()["reason"],
        "Document rev from request body and query string have different values"
    );
    assert_eq!(db.get("a").await.unwrap().data["v"], 1);

    let resp = put(
        &app,
        &format!("/db/a?rev={rev}"),
        json!({"_rev": rev, "v": 2}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
}

#[tokio::test]
async fn if_match_supplies_the_revision() {
    let (db, app, rev) = with_doc().await;
    let etag = format!("\"{rev}\"");

    let resp = send(
        &app,
        req(
            Method::PUT,
            "/db/a",
            &[("if-match", &etag)],
            Some(json!({"v": 2})),
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let rev2 = resp.json()["rev"].as_str().unwrap().to_string();
    assert!(rev2.starts_with("2-"), "{rev2}");
    assert_eq!(resp.json(), json!({"ok": true, "id": "a", "rev": rev2}));
    assert_eq!(resp.header("etag"), Some(format!("\"{rev2}\"").as_str()));
    assert_eq!(db.get("a").await.unwrap().data, json!({"v": 2}));

    let resp = send(
        &app,
        req(
            Method::DELETE,
            "/db/a",
            &[("if-match", &format!("\"{rev2}\""))],
            None,
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let rev3 = resp.json()["rev"].as_str().unwrap().to_string();
    assert!(rev3.starts_with("3-"), "{rev3}");
    assert_eq!(resp.json(), json!({"ok": true, "id": "a", "rev": rev3}));
    assert!(db.get("a").await.is_err());
}

#[tokio::test]
async fn if_match_must_agree_with_other_revisions() {
    let (db, app, rev) = with_doc().await;
    let resp = send(
        &app,
        req(
            Method::DELETE,
            &format!("/db/a?rev={rev}"),
            &[("if-match", "\"1-b\"")],
            None,
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json()["reason"],
        "Document rev and etag have different values"
    );

    let resp = send(
        &app,
        req(
            Method::PUT,
            "/db/a",
            &[("if-match", "\"1-b\"")],
            Some(json!({"_rev": rev})),
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert!(db.get("a").await.is_ok());
}

#[tokio::test]
async fn delete_without_any_revision_is_a_conflict() {
    let (_db, app, _rev) = with_doc().await;
    let resp = delete(&app, "/db/a").await;
    assert_eq!(resp.status, StatusCode::CONFLICT);
    assert_eq!(resp.json()["error"], "conflict");
}

#[tokio::test]
async fn design_docs_use_the_same_revision_rules() {
    let app = app();
    let resp = put(&app, "/db/_design/d", json!({"views": {}})).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let rev = resp.json()["rev"].as_str().unwrap().to_string();
    assert!(rev.starts_with("1-"), "{rev}");
    assert_eq!(
        resp.json(),
        json!({"ok": true, "id": "_design/d", "rev": rev})
    );

    let resp = put(
        &app,
        &format!("/db/_design/d?rev={rev}"),
        json!({"_rev": "1-x"}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);

    let resp = put(
        &app,
        &format!("/db/_design/d?rev={rev}"),
        json!({"views": {}}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let rev2 = resp.json()["rev"].as_str().unwrap().to_string();
    assert!(rev2.starts_with("2-"), "{rev2}");

    let resp = delete(&app, "/db/_design/d").await;
    assert_eq!(resp.status, StatusCode::CONFLICT);
    assert_eq!(resp.json()["error"], "conflict");
    let resp = send(
        &app,
        req(
            Method::DELETE,
            "/db/_design/d",
            &[("if-match", &format!("\"{rev2}\""))],
            None,
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let rev3 = resp.json()["rev"].as_str().unwrap().to_string();
    assert!(rev3.starts_with("3-"), "{rev3}");
    assert_eq!(
        resp.json(),
        json!({"ok": true, "id": "_design/d", "rev": rev3})
    );
    assert_eq!(
        get(&app, "/db/_design/d").await.status,
        StatusCode::NOT_FOUND
    );
}
