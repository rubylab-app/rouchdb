//! HTTP behaviors pinned to what CouchDB 3.5.1 answers for the same request
//! (captured with curl against the reference server; see also the
//! `couchdb_differential` test below, which replays them against a live
//! CouchDB).
mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::*;
use rouchdb::Database;
use rouchdb_server::ServerConfig;
use serde_json::{Value, json};

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
    // The server listens on loopback, so it answers these names only once
    // they are declared (as for a reverse proxy that forwards its Host).
    let config = ServerConfig {
        allowed_hosts: vec!["example.com".into(), "internal".into()],
        ..config()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);
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
    let conflict = request(&app, Method::PUT, "/db/_design/d", &host, Some("{}")).await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
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
    // An unknown revision is `missing`, even for a deleted document.
    assert_error(
        &get(&app, "/db/gone?rev=1-abc").await,
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
    let text = [("content-type", "text/plain")];
    let text_if_match = [("content-type", "text/plain"), ("if-match", "\"garbage\"")];
    // The revision is checked before the document is looked up, so a
    // missing document gets the same 400.
    for resp in [
        get(&app, "/db/doc?rev=garbage").await,
        get(&app, "/db/missing?rev=garbage").await,
        get(&app, "/db/doc/att.txt?rev=garbage").await,
        get(&app, "/db/missing/att.txt?rev=garbage").await,
        put(&app, "/db/doc?rev=garbage", json!({})).await,
        put(&app, "/db/missing?rev=garbage", json!({})).await,
        put(&app, "/db/doc", json!({"_rev": "garbage"})).await,
        delete(&app, "/db/doc?rev=garbage").await,
        request(
            &app,
            Method::PUT,
            "/db/doc/a.txt?rev=garbage",
            &text,
            Some("x"),
        )
        .await,
        request(
            &app,
            Method::PUT,
            "/db/missing/a.txt?rev=garbage",
            &text,
            Some("x"),
        )
        .await,
        request(
            &app,
            Method::PUT,
            "/db/missing/a.txt",
            &text_if_match,
            Some("x"),
        )
        .await,
        delete(&app, "/db/doc/att.txt?rev=garbage").await,
        delete(&app, "/db/missing/att.txt?rev=garbage").await,
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

/// `GET /{db}/{docid}` passes its read options on (CouchDB answers
/// `?attachments=true` as multipart unless asked for JSON; the members are
/// the same).
#[tokio::test]
async fn document_read_options_are_honored() {
    let db = Arc::new(Database::memory(DB));
    let r1 = db.put("c", json!({"v": 1})).await.unwrap().rev.unwrap();
    let r2 = db
        .put_attachment("c", "a.txt", &r1, b"hi".to_vec(), "text/plain")
        .await
        .unwrap()
        .rev
        .unwrap();
    let hash1 = r1.strip_prefix("1-").unwrap();
    let loser = format!("2-{}", "0".repeat(32));
    let conflict = rouchdb::Document::from_json(json!({
        "_id": "c", "_rev": loser, "v": 0,
        "_revisions": {"start": 2, "ids": ["0".repeat(32), hash1]},
    }))
    .unwrap();
    let written = db
        .bulk_docs(vec![conflict], rouchdb::BulkDocsOptions::replication())
        .await
        .unwrap();
    assert!(written[0].ok, "{:?}", written[0]);
    let app = app_with(db, &config());

    let doc = get(&app, "/db/c?conflicts=true").await.json();
    assert_eq!(doc["_rev"], r2.as_str());
    assert_eq!(doc["_conflicts"], json!([loser]));

    let doc = get(&app, "/db/c?revs_info=true").await.json();
    assert_eq!(
        doc["_revs_info"],
        json!([
            {"rev": r2, "status": "available"},
            {"rev": r1, "status": "available"},
        ])
    );

    // `latest` follows the requested revision's branch to its leaf.
    let doc = get(&app, &format!("/db/c?rev={r1}")).await.json();
    assert_eq!(doc["_rev"], r1.as_str());
    let doc = get(&app, &format!("/db/c?rev={r1}&latest=true"))
        .await
        .json();
    assert_eq!(doc["_rev"], r2.as_str());

    let doc = get(&app, "/db/c").await.json();
    assert_eq!(doc["_attachments"]["a.txt"]["stub"], true, "{doc}");
    assert!(doc["_attachments"]["a.txt"].get("data").is_none(), "{doc}");
    let doc = get(&app, "/db/c?attachments=true").await.json();
    assert_eq!(doc["_attachments"]["a.txt"]["data"], "aGk=", "{doc}");
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
    // Valid values still work, 0 included.
    put(&app, "/db/a", json!({})).await;
    put(&app, "/db/b", json!({})).await;
    let resp = get(&app, "/db/_all_docs?limit=0&skip=0").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json()["rows"], json!([]));
    // A range that sorts before every id is empty; descending, the offset
    // is then the number of rows (CouchDB: `offset` == `total_rows`).
    let resp = get(&app, "/db/_all_docs?descending=true&startkey=1").await;
    assert_eq!(
        resp.json(),
        json!({"total_rows": 2, "offset": 2, "rows": []})
    );
    let resp = get(&app, "/db/_all_docs?startkey=1&endkey=1").await;
    assert_eq!(
        resp.json(),
        json!({"total_rows": 2, "offset": 0, "rows": []})
    );
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

    // Filtered feeds: nothing is pending when `limit` was not reached; when
    // it was, the unfiltered changes after the last one returned are.
    let selector = json!({"selector": {"_id": "a"}});
    for (uri, expected) in [
        ("/db/_changes?filter=_selector&limit=5", 0),
        ("/db/_changes?filter=_selector&limit=1", 2),
    ] {
        let body = post(&app, uri, selector.clone()).await.json();
        assert_eq!(body["results"].as_array().unwrap().len(), 1, "{uri}");
        assert_eq!(body["pending"], expected, "{uri}");
    }

    let body = get(&app, "/db/_changes").await.json();
    let results = body["results"].as_array().unwrap();
    for change in &results[..2] {
        assert!(change.get("deleted").is_none(), "{change}");
    }
    assert_eq!(results[2]["id"], "c");
    assert_eq!(results[2]["deleted"], true);
}

// ─── Differential check against a live CouchDB ─────────────────────────────

/// A request replayed against CouchDB and RouchDB.
struct Probe {
    method: Method,
    /// Path below the database, e.g. `"/doc"`; `""` is the database itself.
    path: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: Option<Value>,
    /// Response headers whose presence must agree.
    headers_present: &'static [&'static str],
}

fn probe(method: Method, path: &'static str, body: Option<Value>) -> Probe {
    let headers = if body.is_some() {
        vec![("content-type", "application/json")]
    } else {
        vec![]
    };
    Probe {
        method,
        path,
        headers,
        body,
        headers_present: &[],
    }
}

/// What must agree between the two servers.
fn outcome(status: u16, body: &Value) -> Value {
    let shape = match body {
        Value::Object(o) if o.contains_key("error") => {
            json!({"error": o["error"], "reason": o["reason"]})
        }
        Value::Object(o) if o.contains_key("docs") => {
            json!({"docs": o["docs"].as_array().map(|d| d.iter().map(|d| d["_id"].clone()).collect::<Vec<_>>())})
        }
        Value::Object(o) if o.contains_key("pending") => {
            json!({"n": o["results"].as_array().map(Vec::len), "pending": o["pending"]})
        }
        Value::Object(o) if o.contains_key("rev") => {
            let rev = o["rev"].as_str().unwrap_or_default();
            json!({"ok": o.get("ok"), "id": o["id"], "rev_pos": rev.split('-').next()})
        }
        Value::Array(items) => json!({"array_len": items.len()}),
        other => json!({"ok": other.get("ok")}),
    };
    json!({"status": status, "body": shape})
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn couchdb_differential() {
    let couch_root = std::env::var("COUCHDB_URL")
        .unwrap_or_else(|_| "http://admin:password@localhost:15984".to_string());
    let name = format!("c2_fidelity_{}", uuid_like());
    let couch = format!("{couch_root}/{name}");
    let client = reqwest::Client::new();
    assert!(
        client
            .put(&couch)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );

    let config = ServerConfig {
        db_name: name.clone(),
        ..Default::default()
    };
    let addr = serve(app_with(Arc::new(Database::memory(&name)), &config)).await;
    let ours = format!("http://{addr}/{name}");

    let docs = json!({"docs": [
        {"_id": "a1", "f": 1, "sc": [50, 60]},
        {"_id": "a2", "f": "x", "sc": [50]},
        {"_id": "a3", "f": null, "sc": 50},
        {"_id": "a4", "f": [1, 2], "sc": [50.0]},
        {"_id": "a5", "f": {"k": 1}, "sc": [50.0, 60]},
        {"_id": "mi", "g": 1},
    ]});
    let find = |selector: Value| probe(Method::POST, "/_find", Some(json!({"selector": selector})));
    let mut probes = vec![
        probe(Method::POST, "/_bulk_docs", Some(docs)),
        probe(Method::GET, "/_changes?limit=1", None),
        probe(Method::GET, "/nope", None),
        probe(Method::GET, "/a1?rev=garbage", None),
        probe(Method::GET, "/a1/att?rev=garbage", None),
        probe(Method::PUT, "/a1", Some(json!({}))),
        probe(Method::PUT, "/_bad", Some(json!({}))),
        probe(Method::DELETE, "/nope?rev=1-abc", None),
        probe(Method::GET, "/_design/nope", None),
        Probe {
            headers: vec![("content-type", "text/plain")],
            body: Some(json!("x")),
            ..probe(Method::PUT, "/nope/a.txt?rev=1-abc", None)
        },
        probe(Method::DELETE, "/nope/a.txt?rev=1-abc", None),
        probe(Method::DELETE, "/a1/none.txt?rev=1-abc", None),
        probe(Method::GET, "/nope?rev=garbage", None),
        probe(Method::PUT, "/nope?rev=garbage", Some(json!({}))),
        Probe {
            headers: vec![("content-type", "text/plain")],
            body: Some(json!("x")),
            ..probe(Method::PUT, "/nope/a.txt?rev=garbage", None)
        },
        probe(Method::DELETE, "/nope/a.txt?rev=garbage", None),
        probe(Method::GET, "/_all_docs?limit=abc", None),
        probe(Method::GET, "/_all_docs?skip=-1", None),
        probe(Method::GET, "/_all_docs?descending=abc", None),
        probe(Method::POST, "/_compact", None),
        probe(
            Method::POST,
            "/_bulk_docs",
            Some(json!({"docs": [{"_id": "ok1"}, {"_id": "_bad"}]})),
        ),
        probe(
            Method::POST,
            "/_bulk_docs",
            Some(json!({"docs": [{"_id": "x1", "_foo": 1}]})),
        ),
        probe(
            Method::POST,
            "/_bulk_docs",
            Some(json!({"new_edits": false, "docs": [{"_id": "r1", "_rev": "3-abc"}]})),
        ),
        probe(
            Method::POST,
            "/_bulk_docs",
            Some(json!({"new_edits": false, "docs": [{"_id": "r2"}]})),
        ),
        probe(Method::POST, "/_find", Some(json!({"sel": {}}))),
        find(json!({"a": {"$foo": 1}})),
        find(json!({"f": {"$not": 5}})),
        find(json!({"$not": 5})),
        find(json!({"$gt": 1})),
        find(json!({"a..b": 1})),
        find(json!({"$and": []})),
        find(json!({"$nor": []})),
        find(json!({"$or": []})),
        find(json!({"f": {"$and": []}})),
        find(json!({"$not": {"$or": []}})),
        find(json!({"f": {"$exists": true}, "$or": []})),
        find(json!({"$or": [{"$and": []}, {"f": 1}]})),
        find(json!({"sc": {"$all": [50.0]}})),
        find(json!({"sc": {"$all": [50]}})),
        find(json!({"sc": {"$all": [[50.0]]}})),
        find(json!({"s": {"$regex": "(?=a)"}})),
        Probe {
            headers_present: &["location", "etag"],
            ..probe(Method::PUT, "/newdoc", Some(json!({})))
        },
        Probe {
            headers: vec![("content-type", "text/plain")],
            body: Some(json!("hello")),
            headers_present: &["location"],
            ..probe(Method::PUT, "/attdoc/att.txt", None)
        },
        Probe {
            headers_present: &["etag", "accept-ranges", "content-security-policy"],
            ..probe(Method::GET, "/attdoc/att.txt", None)
        },
        Probe {
            headers_present: &["location"],
            ..probe(Method::PUT, "/_local/l1", Some(json!({})))
        },
        probe(Method::DELETE, "", None),
        probe(Method::GET, "", None),
        probe(Method::GET, "/a1", None),
        probe(Method::PUT, "/a1", Some(json!({}))),
        probe(Method::DELETE, "", None),
        probe(Method::PUT, "", None),
        probe(Method::PUT, "", None),
    ];

    let mut mismatches = Vec::new();
    for p in probes.drain(..) {
        let mut results = Vec::new();
        for base in [&couch, &ours] {
            let mut req = client.request(p.method.clone(), format!("{base}{}", p.path));
            for (k, v) in &p.headers {
                req = req.header(*k, *v);
            }
            req = match &p.body {
                Some(Value::String(s)) => req.body(s.clone()),
                Some(v) => req.body(v.to_string()),
                None => req,
            };
            let resp = req.send().await.unwrap();
            let status = resp.status().as_u16();
            let present: Vec<bool> = p
                .headers_present
                .iter()
                .map(|h| resp.headers().contains_key(*h))
                .collect();
            let text = resp.text().await.unwrap();
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            results.push((outcome(status, &body), present));
        }
        if results[0] != results[1] {
            mismatches.push(format!(
                "{} {}\n  couchdb: {:?}\n  rouchdb: {:?}",
                p.method, p.path, results[0], results[1]
            ));
        }
    }
    let _ = client.delete(&couch).send().await;
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

fn uuid_like() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
