//! Request bodies holding documents as deeply nested as the database stores
//! them (`MAX_NESTING_DEPTH`) are accepted on every route, and deeper ones
//! get the same 400 as a too-deep document write, not axum's 128-level
//! limit.
mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::*;
use rouchdb::{Database, MAX_NESTING_DEPTH};
use serde_json::{Value, json};

/// `{"v": [[...[1]...]]}`, crossing `depth` containers (top level included).
fn nested(depth: usize) -> Value {
    let mut v = json!(1);
    for _ in 1..depth {
        v = Value::Array(vec![v]);
    }
    json!({ "v": v })
}

/// A response body, decoded without serde_json's 128-level limit.
fn deep_json(resp: &Resp) -> Value {
    rouchdb_core::json::from_slice(&resp.body, usize::MAX).unwrap()
}

fn too_deep() -> Value {
    json!({
        "error": "bad_request",
        "reason": format!("Document nesting exceeds the maximum depth of {MAX_NESTING_DEPTH}"),
    })
}

fn with(mut doc: Value, key: &str, value: Value) -> Value {
    doc[key] = value;
    doc
}

#[tokio::test]
async fn documents_up_to_the_limit_are_accepted_on_every_route() {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &config());
    let edge = nested(MAX_NESTING_DEPTH);

    let resp = put(&app, "/db/put", edge.clone()).await;
    assert_eq!(resp.status, StatusCode::CREATED, "{:?}", resp.body);
    let resp = post(&app, "/db", with(edge.clone(), "_id", "posted".into())).await;
    assert_eq!(resp.status, StatusCode::CREATED, "{:?}", resp.body);
    let bulk = json!({"docs": [with(edge.clone(), "_id", "bulk".into())]});
    let resp = post(&app, "/db/_bulk_docs", bulk).await;
    assert_eq!(resp.status, StatusCode::CREATED, "{:?}", resp.body);
    let resp = put(&app, "/db/_local/deep", edge.clone()).await;
    assert_eq!(resp.status, StatusCode::CREATED, "{:?}", resp.body);

    for id in ["put", "posted", "bulk"] {
        let got = deep_json(&get(&app, &format!("/db/{id}")).await);
        assert_eq!(got["v"], edge["v"], "{id}");
        assert_eq!(db.get(id).await.unwrap().data, edge, "{id}");
    }
    assert_eq!(
        deep_json(&get(&app, "/db/_local/deep").await)["v"],
        edge["v"]
    );

    // A selector as deep as the documents finds them.
    let query = json!({"selector": {"v": edge["v"]}, "fields": ["_id"]});
    let resp = post(&app, "/db/_find", query).await;
    assert_eq!(resp.status, StatusCode::OK, "{:?}", resp.body);
    let mut ids: Vec<String> = deep_json(&resp)["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, ["bulk", "posted", "put"]);
}

/// Send `body` as is (built as text: a Value this deep would overflow the
/// test's stack when serialized or dropped).
async fn send_text(app: &axum::Router, method: Method, uri: &str, body: String) -> Resp {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    send(app, req).await
}

#[tokio::test]
async fn deeper_documents_get_the_write_error() {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &config());
    for depth in [
        MAX_NESTING_DEPTH + 1,
        MAX_NESTING_DEPTH + 2,
        5 * MAX_NESTING_DEPTH,
    ] {
        // `{<members>"v": [[...[1]...]]}`, `depth` containers deep.
        let doc = |members: &str| {
            format!(
                r#"{{{members}"v":{}1{}}}"#,
                "[".repeat(depth - 1),
                "]".repeat(depth - 1)
            )
        };
        let mut responses = vec![
            send_text(&app, Method::PUT, "/db/put", doc("")).await,
            send_text(&app, Method::POST, "/db", doc(r#""_id":"posted","#)).await,
            send_text(
                &app,
                Method::POST,
                "/db/_bulk_docs",
                format!(r#"{{"docs":[{}]}}"#, doc(r#""_id":"bulk","#)),
            )
            .await,
            send_text(&app, Method::PUT, "/db/_local/deep", doc("")).await,
        ];
        // A design document keeps only the members it knows, so only a
        // body too deep to decode fails there.
        if depth > MAX_NESTING_DEPTH + 2 {
            let design = doc(r#""views":{},"#);
            responses.push(send_text(&app, Method::PUT, "/db/_design/deep", design).await);
        }
        for (i, resp) in responses.iter().enumerate() {
            assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{depth} #{i}");
            assert_eq!(resp.json(), too_deep(), "{depth} #{i}");
        }
    }
    let info = db.info().await.unwrap();
    assert_eq!((info.doc_count, info.update_seq.as_num()), (0, 0));
    assert!(db.get_design("deep").await.is_err());
    assert!(db.adapter().get_local("deep").await.is_err());

    // Other malformed bodies keep their errors.
    let resp = post(&app, "/db/_bulk_docs", json!({"docs": 5})).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json()["error"], "bad_request");
    let resp = send_text(&app, Method::POST, "/db/_bulk_docs", "{\"docs\":".into()).await;
    assert_eq!(
        resp.json(),
        json!({"error": "bad_request", "reason": "invalid UTF-8 JSON"})
    );
}
