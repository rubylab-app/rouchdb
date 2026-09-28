//! HTTP behaviors pinned to what CouchDB 3.5.1 answers for the same request
//! (captured with curl against the reference server).
mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::*;
use rouchdb::Database;
use serde_json::json;

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Resp {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    send(app, req.body(body).unwrap()).await
}

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

// ─── Documents and attachments ──────────────────────────────────────────────

#[tokio::test]
async fn attachment_put_on_missing_doc_creates_the_first_revision() {
    let app = app();
    let resp = request(
        &app,
        Method::PUT,
        "/db/newdoc/att.txt",
        &[("content-type", "text/plain")],
        Some("hello"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let rev = resp.json()["rev"].as_str().unwrap().to_string();
    assert!(rev.starts_with("1-"), "{rev}");

    let doc = get(&app, "/db/newdoc").await.json();
    assert_eq!(doc["_rev"], rev);
    assert_eq!(doc["_attachments"]["att.txt"]["content_type"], "text/plain");
    assert_eq!(doc["_attachments"]["att.txt"]["length"], 5);
    let resp = get(&app, "/db/newdoc/att.txt").await;
    assert_eq!(&resp.body[..], b"hello");
    // One revision only: nothing older than 1- in the history.
    let revs = get(&app, "/db/newdoc?revs=true").await.json();
    assert_eq!(revs["_revisions"]["start"], 1);
    assert_eq!(revs["_revisions"]["ids"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn write_responses_carry_location_and_etag() {
    let app = app();
    let host = [("host", "example.com:5984")];

    let resp = request(&app, Method::PUT, "/db/a%20b", &host, Some("{}")).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let rev = resp.json()["rev"].as_str().unwrap().to_string();
    assert_eq!(
        resp.header("location"),
        Some("http://example.com:5984/db/a%20b")
    );
    assert_eq!(resp.header("etag"), Some(format!("\"{rev}\"").as_str()));

    let resp = request(
        &app,
        Method::POST,
        "/db",
        &[
            ("host", "example.com:5984"),
            ("content-type", "application/json"),
        ],
        Some(r#"{"_id": "x:y+z"}"#),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(
        resp.header("location"),
        Some("http://example.com:5984/db/x:y%2Bz")
    );

    let resp = request(&app, Method::PUT, "/db/_design/d", &host, Some("{}")).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(
        resp.header("location"),
        Some("http://example.com:5984/db/_design%2Fd")
    );
    assert!(resp.header("etag").is_some());

    let resp = request(&app, Method::PUT, "/db/_local/l1", &host, Some("{}")).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(
        resp.header("location"),
        Some("http://example.com:5984/db/_local%2Fl1")
    );

    let resp = request(
        &app,
        Method::PUT,
        &format!("/db/a%20b/at%20t.txt?rev={rev}"),
        &[("host", "example.com:5984"), ("content-type", "text/plain")],
        Some("hello"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(
        resp.header("location"),
        Some("http://example.com:5984/db/a%20b/at%20t.txt")
    );
    let rev = resp.json()["rev"].as_str().unwrap().to_string();

    // X-Forwarded-Host / X-Forwarded-Proto win, as in CouchDB.
    let resp = request(
        &app,
        Method::PUT,
        "/db/fwd",
        &[
            ("host", "internal:5984"),
            ("x-forwarded-host", "db.example"),
            ("x-forwarded-proto", "https"),
        ],
        Some("{}"),
    )
    .await;
    assert_eq!(resp.header("location"), Some("https://db.example/db/fwd"));
    let resp = request(
        &app,
        Method::PUT,
        "/db/plain",
        &[("host", "internal:5984"), ("x-forwarded-proto", "http")],
        Some("{}"),
    )
    .await;
    assert_eq!(
        resp.header("location"),
        Some("http://internal:5984/db/plain")
    );

    // Without a Host header the Location is relative.
    let resp = put(&app, "/db/nohost", json!({})).await;
    assert_eq!(resp.header("location"), Some("/db/nohost"));

    let resp = delete(&app, &format!("/db/a%20b?rev={rev}")).await;
    assert_eq!(resp.status, StatusCode::OK);
    let del_rev = resp.json()["rev"].as_str().unwrap().to_string();
    assert!(del_rev.starts_with("3-"));
    assert_eq!(resp.header("etag"), Some(format!("\"{del_rev}\"").as_str()));

    let resp = call(&app, Method::DELETE, "/db", None).await;
    assert_eq!(resp.status, StatusCode::OK);
    let resp = request(&app, Method::PUT, "/db", &host, None).await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(resp.header("location"), Some("http://example.com:5984/db"));
}

#[tokio::test]
async fn attachment_get_sends_digest_etag_and_accept_ranges() {
    let app = app();
    let resp = request(
        &app,
        Method::PUT,
        "/db/doc/att.txt",
        &[("content-type", "text/plain")],
        Some("hello"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let digest = get(&app, "/db/doc").await.json()["_attachments"]["att.txt"]["digest"]
        .as_str()
        .unwrap()
        .to_string();
    let md5 = digest.strip_prefix("md5-").unwrap();

    for method in [Method::GET, Method::HEAD] {
        let resp = request(&app, method.clone(), "/db/doc/att.txt", &[], None).await;
        assert_eq!(resp.status, StatusCode::OK, "{method}");
        assert_eq!(resp.header("etag"), Some(format!("\"{md5}\"").as_str()));
        assert_eq!(resp.header("accept-ranges"), Some("none"));
        assert_eq!(resp.header("content-security-policy"), Some("sandbox"));
        assert_eq!(resp.header("content-type"), Some("text/plain"));
    }
}

#[tokio::test]
async fn missing_and_deleted_documents_have_couchdb_reasons() {
    let db = Arc::new(Database::memory(DB));
    let rev = db.put("gone", json!({})).await.unwrap().rev.unwrap();
    db.remove("gone", &rev).await.unwrap();
    let app = app_with(db, &config());

    assert_error(
        &get(&app, "/db/gone").await,
        StatusCode::NOT_FOUND,
        "not_found",
        "deleted",
    );
    assert_error(
        &get(&app, "/db/nope").await,
        StatusCode::NOT_FOUND,
        "not_found",
        "missing",
    );
    assert_error(
        &get(&app, "/db/nope/att.txt").await,
        StatusCode::NOT_FOUND,
        "not_found",
        "missing",
    );
}

#[tokio::test]
async fn writes_on_missing_documents_answer_like_couchdb() {
    let app = app();
    assert_error(
        &delete(&app, "/db/nope?rev=1-abc").await,
        StatusCode::NOT_FOUND,
        "not_found",
        "missing",
    );
    assert_error(
        &get(&app, "/db/_design/nope").await,
        StatusCode::NOT_FOUND,
        "not_found",
        "missing",
    );
    // An edit of a revision that does not exist is a conflict.
    assert_error(
        &put(&app, "/db/nope", json!({"_rev": "1-abc"})).await,
        StatusCode::CONFLICT,
        "conflict",
        "Document update conflict.",
    );
    // CouchDB's attachment endpoints answer 409 with a `not_found` error.
    for method in [Method::PUT, Method::DELETE] {
        let resp = request(
            &app,
            method,
            "/db/nope/a.txt?rev=1-abc",
            &[("content-type", "text/plain")],
            Some("x"),
        )
        .await;
        assert_error(&resp, StatusCode::CONFLICT, "not_found", "missing_rev");
    }
    assert_eq!(get(&app, "/db/nope").await.status, StatusCode::NOT_FOUND);

    let rev = put(&app, "/db/doc", json!({})).await.json()["rev"]
        .as_str()
        .unwrap()
        .to_string();
    assert_error(
        &delete(&app, &format!("/db/doc/none.txt?rev={rev}")).await,
        StatusCode::NOT_FOUND,
        "not_found",
        "Document is missing attachment",
    );
    // An unknown revision of an existing document is `missing_rev` too; a
    // known but stale one is a plain conflict.
    for method in [Method::PUT, Method::DELETE] {
        let resp = request(
            &app,
            method,
            "/db/doc/a.txt?rev=1-abc",
            &[("content-type", "text/plain")],
            Some("x"),
        )
        .await;
        assert_error(&resp, StatusCode::CONFLICT, "not_found", "missing_rev");
    }
    let resp = request(
        &app,
        Method::PUT,
        &format!("/db/doc/a.txt?rev={rev}"),
        &[("content-type", "text/plain")],
        Some("x"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let resp = request(
        &app,
        Method::PUT,
        &format!("/db/doc/b.txt?rev={rev}"),
        &[("content-type", "text/plain")],
        Some("x"),
    )
    .await;
    assert_error(
        &resp,
        StatusCode::CONFLICT,
        "conflict",
        "Document update conflict.",
    );
}

// ─── Error statuses, names and reasons ──────────────────────────────────────

#[tokio::test]
async fn invalid_revisions_are_bad_requests() {
    let db = Arc::new(Database::memory(DB));
    db.put("doc", json!({})).await.unwrap();
    let app = app_with(db, &config());
    for resp in [
        get(&app, "/db/doc?rev=garbage").await,
        get(&app, "/db/doc/att.txt?rev=garbage").await,
        delete(&app, "/db/doc?rev=garbage").await,
        put(&app, "/db/doc", json!({"_rev": "garbage"})).await,
    ] {
        assert_error(
            &resp,
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Invalid rev format",
        );
    }
    // A well-formed but unknown revision is simply missing.
    assert_error(
        &get(&app, "/db/doc?rev=1-abc").await,
        StatusCode::NOT_FOUND,
        "not_found",
        "missing",
    );
}

#[tokio::test]
async fn content_type_and_method_errors() {
    let app = app();
    // _compact requires a JSON content type, as in CouchDB.
    let resp = request(&app, Method::POST, "/db/_compact", &[], None).await;
    assert_error(
        &resp,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "bad_content_type",
        "Content-Type must be application/json",
    );
    let json_ct = [("content-type", "application/json")];
    let resp = request(&app, Method::POST, "/db/_compact", &json_ct, None).await;
    assert_eq!(resp.status, StatusCode::ACCEPTED);

    let resp = request(
        &app,
        Method::POST,
        "/db",
        &[("content-type", "text/plain")],
        Some("{}"),
    )
    .await;
    assert_error(
        &resp,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "bad_content_type",
        "Content-Type must be application/json",
    );

    let resp = request(&app, Method::PATCH, "/db", &[], None).await;
    assert_error(
        &resp,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "Only DELETE,GET,HEAD,POST,PUT allowed",
    );
    assert_eq!(resp.header("allow"), Some("DELETE,GET,HEAD,POST,PUT"));
    let resp = request(&app, Method::POST, "/db/doc", &[], None).await;
    assert_error(
        &resp,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "Only DELETE,GET,HEAD,PUT allowed",
    );
}

#[tokio::test]
async fn conflict_and_uuid_formats() {
    let app = app();
    put(&app, "/db/doc", json!({})).await;
    assert_error(
        &put(&app, "/db/doc", json!({})).await,
        StatusCode::CONFLICT,
        "conflict",
        "Document update conflict.",
    );
    let resp = post(&app, "/db/_bulk_docs", json!({"docs": [{"_id": "doc"}]})).await;
    assert_eq!(
        resp.json(),
        json!([{"id": "doc", "error": "conflict", "reason": "Document update conflict."}])
    );

    let uuids = get(&app, "/_uuids?count=3").await.json();
    for uuid in uuids["uuids"].as_array().unwrap() {
        let uuid = uuid.as_str().unwrap();
        assert_eq!(uuid.len(), 32, "{uuid}");
        assert!(
            uuid.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }
}

#[tokio::test]
async fn all_docs_query_parse_errors() {
    let app = app();
    for (query, reason) in [
        ("limit=abc", r#"Invalid value for integer: "abc""#),
        ("limit=1.5", r#"Invalid value for integer: "1.5""#),
        ("limit=-1", r#"Invalid value for positive integer: "-1""#),
        ("skip=abc", r#"Invalid value for integer: "abc""#),
        ("skip=-1", r#"Invalid value for positive integer: "-1""#),
        ("descending=abc", r#"Invalid boolean parameter: "abc""#),
        ("include_docs=yes", r#"Invalid boolean parameter: "yes""#),
    ] {
        let resp = get(&app, &format!("/db/_all_docs?{query}")).await;
        assert_error(&resp, StatusCode::BAD_REQUEST, "query_parse_error", reason);
    }
    // Valid values still work.
    put(&app, "/db/a", json!({})).await;
    put(&app, "/db/b", json!({})).await;
    let resp = get(
        &app,
        "/db/_all_docs?limit=1&skip=1&descending=true&include_docs=true",
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json()["rows"][0]["id"], "a");
    assert_eq!(resp.json()["rows"][0]["doc"]["_id"], "a");
}

#[tokio::test]
async fn find_errors_have_couchdb_names() {
    let app = app();
    for (body, error, reason) in [
        (
            json!({"selector": {"a": {"$foo": 1}}}),
            "invalid_operator",
            "Invalid operator: $foo",
        ),
        (
            json!({"sel": {}}),
            "missing_required_key",
            "Missing required key: selector",
        ),
        (
            json!({"selector": {"a": {"$in": 1}}}),
            "bad_arg",
            "Bad argument for operator $in: 1",
        ),
        (
            json!({"selector": {"f": {"$not": 5}}}),
            "bad_arg",
            "Bad argument for operator $not: 5",
        ),
        (
            json!({"selector": {"$gt": 1}}),
            "invalid_selector",
            "One or more conditions is missing a field name.",
        ),
        (
            json!({"selector": {"a..b": 1}}),
            "invalid_field_name",
            "Invalid field name: a..b",
        ),
        (
            json!({"selector": 5}),
            "invalid_selector_json",
            "Selector must be a JSON object, not: 5",
        ),
    ] {
        let resp = post(&app, "/db/_find", body.clone()).await;
        assert_error(&resp, StatusCode::BAD_REQUEST, error, reason);
    }
    let resp = post(
        &app,
        "/db/_find",
        json!({"selector": {}, "sort": [{"a": "up"}]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json()["error"], "invalid_sort_field");
    let resp = post(
        &app,
        "/db/_find",
        json!({"selector": {"a": {"$regex": "[a"}}}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json()["error"], "bad_arg");
}

#[tokio::test]
async fn illegal_document_ids_use_illegal_docid() {
    let app = app();
    for resp in [
        put(&app, "/db/_bad", json!({})).await,
        post(&app, "/db", json!({"_id": "_bad"})).await,
    ] {
        assert_error(
            &resp,
            StatusCode::BAD_REQUEST,
            "illegal_docid",
            "Only reserved document ids may start with underscore.",
        );
    }
}

// ─── _bulk_docs ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn bulk_docs_rejects_invalid_documents_as_a_whole() {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &config());

    for (docs, error, reason) in [
        (
            json!([{"_id": "ok1"}, {"_id": "_bad"}]),
            "illegal_docid",
            "Only reserved document ids may start with underscore.",
        ),
        (
            json!([{"_id": "ok1"}, {"_id": ""}]),
            "illegal_docid",
            "Document id must not be empty",
        ),
        (
            json!([{"_id": "ok1"}, {"_id": 5}]),
            "illegal_docid",
            "Document id must be a string",
        ),
        (
            json!([{"_id": "ok1"}, {"_id": "x1", "_foo": 1}]),
            "doc_validation",
            "Bad special document member: _foo",
        ),
    ] {
        let resp = post(&app, "/db/_bulk_docs", json!({ "docs": docs })).await;
        assert_error(&resp, StatusCode::BAD_REQUEST, error, reason);
    }
    // No document of a rejected request was written.
    assert_eq!(db.info().await.unwrap().doc_count, 0);
}

#[tokio::test]
async fn bulk_docs_without_new_edits_reports_only_errors() {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &config());

    let resp = post(
        &app,
        "/db/_bulk_docs",
        json!({"new_edits": false, "docs": [
            {"_id": "r1", "_rev": "3-abc", "k": 1},
            {"_id": "r2", "_revisions": {"start": 2, "ids": ["def", "abc"]}},
        ]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(resp.json(), json!([]));
    assert_eq!(get(&app, "/db/r1").await.json()["_rev"], "3-abc");
    assert_eq!(get(&app, "/db/r2").await.json()["_rev"], "2-def");

    let resp = post(
        &app,
        "/db/_bulk_docs",
        json!({"new_edits": false, "docs": [{"_id": "r3", "k": 1}]}),
    )
    .await;
    assert_error(
        &resp,
        StatusCode::BAD_REQUEST,
        "bad_request",
        "When `new_edits: false`, the document needs `_rev` or `_revisions` specified",
    );
    assert_eq!(get(&app, "/db/r3").await.status, StatusCode::NOT_FOUND);
}

// ─── _changes ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn changes_report_pending_and_omit_deleted_false() {
    let db = Arc::new(Database::memory(DB));
    for id in ["a", "b", "c"] {
        db.put(id, json!({})).await.unwrap();
    }
    let rev = db.get("c").await.unwrap().rev.unwrap().to_string();
    db.remove("c", &rev).await.unwrap();
    let app = app_with(db, &config());

    let pending = |uri: &'static str| {
        let app = app.clone();
        async move {
            let body = get(&app, uri).await.json();
            (
                body["results"].as_array().unwrap().len(),
                body["pending"].clone(),
            )
        }
    };
    assert_eq!(pending("/db/_changes?limit=1").await, (1, json!(2)));
    assert_eq!(pending("/db/_changes?limit=2").await, (2, json!(1)));
    assert_eq!(pending("/db/_changes?limit=3").await, (3, json!(0)));
    assert_eq!(pending("/db/_changes").await, (3, json!(0)));
    assert_eq!(pending("/db/_changes?since=1&limit=1").await, (1, json!(1)));
    assert_eq!(
        pending("/db/_changes?descending=true&limit=1").await,
        (1, json!(2))
    );

    let body = get(&app, "/db/_changes").await.json();
    let results = body["results"].as_array().unwrap();
    for change in &results[..2] {
        assert!(change.get("deleted").is_none(), "{change}");
    }
    assert_eq!(results[2]["id"], "c");
    assert_eq!(results[2]["deleted"], true);
}
