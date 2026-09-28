//! F108: errors are CouchDB-style JSON and PUT accepts any Content-Type.
//! F61: the request body limit is configurable and large enough for
//! attachments and replication batches.
mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::*;
use rouchdb::Database;
use rouchdb_server::ServerConfig;
use serde_json::json;

async fn raw(app: &axum::Router, method: Method, uri: &str, ct: Option<&str>, body: &str) -> Resp {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(ct) = ct {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    send(app, req.body(Body::from(body.to_string())).unwrap()).await
}

fn assert_json_error(resp: &Resp, status: StatusCode, error: &str) {
    assert_eq!(
        resp.status,
        status,
        "{:?}",
        String::from_utf8_lossy(&resp.body)
    );
    assert!(
        resp.header("content-type")
            .is_some_and(|ct| ct.starts_with("application/json")),
        "content-type: {:?}",
        resp.header("content-type")
    );
    let body = resp.json();
    assert_eq!(body["error"], error, "{body}");
    assert!(body["reason"].is_string(), "{body}");
}

#[tokio::test]
async fn put_doc_accepts_any_content_type() {
    let app = app();
    for (id, ct) in [("a", Some("text/plain")), ("b", None)] {
        let resp = raw(&app, Method::PUT, &format!("/db/{id}"), ct, r#"{"x":1}"#).await;
        assert_eq!(resp.status, StatusCode::CREATED, "{ct:?}");
        assert_eq!(get(&app, &format!("/db/{id}")).await.json()["x"], 1);
    }
    let resp = raw(
        &app,
        Method::PUT,
        "/db/_design/d",
        Some("text/plain"),
        r#"{"views":{}}"#,
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
}

#[tokio::test]
async fn put_doc_rejects_invalid_bodies_with_json_errors() {
    let app = app();
    let resp = raw(
        &app,
        Method::PUT,
        "/db/a",
        Some("application/json"),
        r#"{"x":"#,
    )
    .await;
    assert_json_error(&resp, StatusCode::BAD_REQUEST, "bad_request");

    let resp = raw(
        &app,
        Method::PUT,
        "/db/a",
        Some("application/json"),
        "[1,2]",
    )
    .await;
    assert_json_error(&resp, StatusCode::BAD_REQUEST, "bad_request");
    assert_eq!(resp.json()["reason"], "Document must be a JSON object");
}

#[tokio::test]
async fn extractor_rejections_are_json() {
    let app = app();

    // POST still requires application/json, as in CouchDB.
    let resp = raw(&app, Method::POST, "/db", Some("text/plain"), r#"{"x":1}"#).await;
    assert_json_error(
        &resp,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "bad_content_type",
    );

    let resp = raw(
        &app,
        Method::POST,
        "/db/_bulk_docs",
        Some("application/json"),
        r#"{"docs":"#,
    )
    .await;
    assert_json_error(&resp, StatusCode::BAD_REQUEST, "bad_request");

    // Well-formed JSON of the wrong shape: 400, not axum's 422.
    let resp = post(&app, "/db/_bulk_docs", json!({"docs": 5})).await;
    assert_json_error(&resp, StatusCode::BAD_REQUEST, "bad_request");

    let resp = get(&app, "/db/_all_docs?limit=abc").await;
    assert_json_error(&resp, StatusCode::BAD_REQUEST, "bad_request");
}

#[tokio::test]
async fn unsupported_method_is_json_405() {
    let app = app();
    let resp = raw(&app, Method::PATCH, "/db", None, "").await;
    assert_json_error(&resp, StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
    assert_eq!(resp.header("allow"), Some("GET,HEAD,PUT,POST,DELETE"));
}

#[tokio::test]
async fn default_body_limit_fits_large_attachments_and_batches() {
    let db = Arc::new(Database::memory(DB));
    let rev = db.put("doc", json!({})).await.unwrap().rev.unwrap();
    let app = app_with(db, &config());

    let jpeg = vec![0xFFu8; 3 * 1024 * 1024];
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/db/doc/photo.jpg?rev={rev}"))
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::from(jpeg))
        .unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::CREATED);

    let docs: Vec<_> = (0..300)
        .map(|i| json!({"_id": format!("d{i}"), "pad": "x".repeat(10_000)}))
        .collect();
    let resp = post(&app, "/db/_bulk_docs", json!({ "docs": docs })).await;
    assert_eq!(resp.status, StatusCode::CREATED);
}

#[tokio::test]
async fn body_limit_is_configurable_and_reported_as_json() {
    let config = ServerConfig {
        max_request_size: 1024,
        ..config()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);

    let big = json!({ "pad": "x".repeat(2048) });
    let resp = put(&app, "/db/doc", big.clone()).await;
    assert_json_error(&resp, StatusCode::PAYLOAD_TOO_LARGE, "too_large");
    let resp = post(&app, "/db/_bulk_docs", json!({ "docs": [big] })).await;
    assert_json_error(&resp, StatusCode::PAYLOAD_TOO_LARGE, "too_large");

    let resp = put(&app, "/db/small", json!({"a": 1})).await;
    assert_eq!(resp.status, StatusCode::CREATED);
}
