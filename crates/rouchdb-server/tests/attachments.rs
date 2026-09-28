//! F110: attachment GET honors `?rev`, PUT without rev creates the doc,
//! and attachment names may contain `/`.
mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::*;
use rouchdb::Database;
use serde_json::json;

async fn put_att(
    app: &axum::Router,
    uri: &str,
    ct: &str,
    data: &str,
    headers: &[(&str, &str)],
) -> Resp {
    let mut req = Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .header(header::CONTENT_TYPE, ct);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    send(app, req.body(Body::from(data.to_string())).unwrap()).await
}

fn rev_of(resp: &Resp) -> String {
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "{:?}",
        String::from_utf8_lossy(&resp.body)
    );
    resp.json()["rev"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn get_attachment_honors_rev() {
    let db = Arc::new(Database::memory(DB));
    let r1 = db.put("doc", json!({})).await.unwrap().rev.unwrap();
    let app = app_with(db, &config());

    let r2 = rev_of(
        &put_att(
            &app,
            &format!("/db/doc/f.txt?rev={r1}"),
            "text/plain",
            "hello",
            &[],
        )
        .await,
    );
    let r3 = rev_of(
        &put_att(
            &app,
            &format!("/db/doc/f.txt?rev={r2}"),
            "text/markdown",
            "world",
            &[],
        )
        .await,
    );

    let resp = get(&app, &format!("/db/doc/f.txt?rev={r2}")).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(&resp.body[..], b"hello");
    assert_eq!(resp.header("content-type"), Some("text/plain"));

    let resp = get(&app, "/db/doc/f.txt").await;
    assert_eq!(&resp.body[..], b"world");
    assert_eq!(resp.header("content-type"), Some("text/markdown"));
    let resp = get(&app, &format!("/db/doc/f.txt?rev={r3}")).await;
    assert_eq!(&resp.body[..], b"world");

    let resp = get(&app, "/db/doc/f.txt?rev=9-nope").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    assert_eq!(resp.json()["error"], "not_found");
    let resp = get(&app, "/db/doc/nope.txt").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn put_attachment_without_rev_creates_the_document() {
    let app = app();
    let resp = put_att(&app, "/db/newdoc/f.txt", "text/plain", "hi", &[]).await;
    rev_of(&resp);
    assert_eq!(resp.json()["id"], "newdoc");

    let doc = get(&app, "/db/newdoc").await.json();
    assert_eq!(doc["_attachments"]["f.txt"]["content_type"], "text/plain");
    assert_eq!(&get(&app, "/db/newdoc/f.txt").await.body[..], b"hi");

    // On an existing document a revision is required.
    let resp = put_att(&app, "/db/newdoc/g.txt", "text/plain", "x", &[]).await;
    assert_eq!(resp.status, StatusCode::CONFLICT);
    assert_eq!(resp.json()["error"], "conflict");
}

#[tokio::test]
async fn attachment_revision_from_if_match() {
    let db = Arc::new(Database::memory(DB));
    let r1 = db.put("doc", json!({})).await.unwrap().rev.unwrap();
    let app = app_with(db, &config());

    let etag = format!("\"{r1}\"");
    let r2 = rev_of(
        &put_att(
            &app,
            "/db/doc/i.txt",
            "text/plain",
            "x",
            &[("if-match", &etag)],
        )
        .await,
    );

    let resp = send(
        &app,
        Request::builder()
            .method(Method::DELETE)
            .uri("/db/doc/i.txt")
            .header("if-match", format!("\"{r2}\""))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        delete(&app, "/db/doc/i.txt").await.status,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn attachment_names_may_contain_slashes() {
    let db = Arc::new(Database::memory(DB));
    let r1 = db.put("doc", json!({})).await.unwrap().rev.unwrap();
    let app = app_with(db, &config());

    rev_of(
        &put_att(
            &app,
            &format!("/db/doc/dir/h.txt?rev={r1}"),
            "text/plain",
            "nested",
            &[],
        )
        .await,
    );

    let doc = get(&app, "/db/doc").await.json();
    assert!(doc["_attachments"]["dir/h.txt"].is_object(), "{doc}");
    assert_eq!(&get(&app, "/db/doc/dir/h.txt").await.body[..], b"nested");
    assert_eq!(&get(&app, "/db/doc/dir%2Fh.txt").await.body[..], b"nested");
}
