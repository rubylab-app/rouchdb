//! F18: `_all_docs` key parameters are JSON, as in CouchDB.
mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use rouchdb::Database;
use serde_json::json;

async fn seeded() -> (Arc<Database>, axum::Router) {
    let db = Arc::new(Database::memory(DB));
    for id in ["a", "b", "c", "_design/x"] {
        db.put(id, json!({})).await.unwrap();
    }
    let app = app_with(db.clone(), &config());
    (db, app)
}

fn ids(resp: &Resp) -> Vec<String> {
    resp.json()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn key_is_json_decoded() {
    let (db, app) = seeded().await;
    let resp = get(&app, &format!("/db/_all_docs?key={}", q(json!("b")))).await;
    assert_eq!(resp.status, StatusCode::OK);
    let rev = db.get("b").await.unwrap().rev.unwrap().to_string();
    let body = resp.json();
    assert_eq!(body["total_rows"], 4);
    assert_eq!(
        body["rows"],
        json!([{"id": "b", "key": "b", "value": {"rev": rev}}])
    );
}

#[tokio::test]
async fn start_and_end_keys_are_json_decoded() {
    let (_db, app) = seeded().await;
    let uri = format!(
        "/db/_all_docs?startkey={}&endkey={}",
        q(json!("b")),
        q(json!("c"))
    );
    assert_eq!(ids(&get(&app, &uri).await), ["b", "c"]);

    let uri = format!("/db/_all_docs?start_key={}", q(json!("b")));
    assert_eq!(ids(&get(&app, &uri).await), ["b", "c"]);

    let uri = format!(
        "/db/_all_docs?startkey={}&endkey={}&descending=true",
        q(json!("c")),
        q(json!("b"))
    );
    assert_eq!(ids(&get(&app, &uri).await), ["c", "b"]);
}

#[tokio::test]
async fn design_doc_range_lists_design_docs() {
    // The range Fauxton uses to list design documents.
    let (_db, app) = seeded().await;
    let uri = format!(
        "/db/_all_docs?startkey={}&endkey={}",
        q(json!("_design/")),
        q(json!("_design0"))
    );
    assert_eq!(ids(&get(&app, &uri).await), ["_design/x"]);
}

#[tokio::test]
async fn invalid_json_key_is_a_bad_request() {
    let (_db, app) = seeded().await;
    for uri in [
        "/db/_all_docs?key=b",
        "/db/_all_docs?startkey=b",
        "/db/_all_docs?endkey=c",
    ] {
        let resp = get(&app, uri).await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(
            resp.json(),
            json!({"error": "bad_request", "reason": "invalid UTF-8 JSON"}),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn non_string_keys_sort_before_every_doc_id() {
    // CouchDB: in _all_docs every non-string key collates before all ids.
    let (_db, app) = seeded().await;
    let all = ["_design/x", "a", "b", "c"];

    let resp = get(&app, &format!("/db/_all_docs?key={}", q(json!(1)))).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(ids(&resp).is_empty());
    assert_eq!(resp.json()["total_rows"], 4);

    let resp = get(&app, &format!("/db/_all_docs?startkey={}", q(json!(1)))).await;
    assert_eq!(ids(&resp), all);
    let resp = get(&app, &format!("/db/_all_docs?endkey={}", q(json!({})))).await;
    assert!(ids(&resp).is_empty());
    let uri = format!("/db/_all_docs?endkey={}&descending=true", q(json!(null)));
    assert_eq!(ids(&get(&app, &uri).await), ["c", "b", "a", "_design/x"]);
    let uri = format!("/db/_all_docs?startkey={}&descending=true", q(json!(null)));
    assert!(ids(&get(&app, &uri).await).is_empty());
}

/// The `key` of every row (error rows included).
fn keys(resp: &Resp) -> Vec<serde_json::Value> {
    resp.json()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].clone())
        .collect()
}

#[tokio::test]
async fn keys_query_parameter() {
    let (_db, app) = seeded().await;
    let resp = get(
        &app,
        &format!("/db/_all_docs?keys={}", q(json!(["c", "a", 1]))),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(keys(&resp), [json!("c"), json!("a"), json!(1)]);
    assert_eq!(
        resp.json()["rows"][2],
        json!({"key": 1, "error": "not_found"})
    );

    let resp = get(&app, &format!("/db/_all_docs?keys={}", q(json!("a")))).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json(),
        json!({"error": "bad_request", "reason": "`keys` parameter must be an array."})
    );
}

#[tokio::test]
async fn keys_in_post_body() {
    let (_db, app) = seeded().await;
    let resp = post(&app, "/db/_all_docs", json!({"keys": ["b", 7, "a"]})).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(keys(&resp), [json!("b"), json!(7), json!("a")]);

    let resp = post(&app, "/db/_all_docs", json!({})).await;
    assert_eq!(ids(&resp), ["_design/x", "a", "b", "c"]);

    let resp = post(&app, "/db/_all_docs", json!({"keys": "a"})).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json(),
        json!({"error": "bad_request", "reason": "`keys` body member must be an array."})
    );
}

#[tokio::test]
async fn keys_rows_match_couchdb() {
    // CouchDB 3.5.1 answers `keys` with one row per key, in order: live
    // docs, deleted docs (`"doc": null` under include_docs) and a
    // `not_found` row for an unknown id or a non-string key; `offset` is
    // null, and skip/limit/descending apply to that list of rows.
    let (db, app) = seeded().await;
    let ra = db.get("a").await.unwrap().rev.unwrap().to_string();
    let rb = db.get("b").await.unwrap().rev.unwrap().to_string();
    let rb = db.remove("b", &rb).await.unwrap().rev.unwrap();
    let resp = post(
        &app,
        "/db/_all_docs?include_docs=true",
        json!({"keys": ["a", "b", 1, "zz"]}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({"total_rows": 3, "offset": null, "rows": [
            {"id": "a", "key": "a", "value": {"rev": ra}, "doc": {"_id": "a", "_rev": ra}},
            {"id": "b", "key": "b", "value": {"rev": rb, "deleted": true}, "doc": null},
            {"key": 1, "error": "not_found"},
            {"key": "zz", "error": "not_found"},
        ]})
    );

    // Without include_docs there is no `doc` member at all.
    let resp = post(&app, "/db/_all_docs", json!({"keys": ["b"]})).await;
    assert_eq!(
        resp.json()["rows"],
        json!([{"id": "b", "key": "b", "value": {"rev": rb, "deleted": true}}])
    );

    // Descending reverses the keys: [c, "zz", 1, a].
    let resp = post(
        &app,
        "/db/_all_docs?descending=true",
        json!({"keys": ["a", 1, "zz", "c"]}),
    )
    .await;
    assert_eq!(keys(&resp), [json!("c"), json!("zz"), json!(1), json!("a")]);
    assert_eq!(resp.json()["rows"][3]["value"]["rev"], json!(ra));
    // Skip and limit apply to that list, error rows included.
    let resp = post(
        &app,
        "/db/_all_docs?descending=true&skip=1&limit=2",
        json!({"keys": ["a", 1, "zz", "c"]}),
    )
    .await;
    assert_eq!(
        resp.json()["rows"],
        json!([{"key": "zz", "error": "not_found"}, {"key": 1, "error": "not_found"}])
    );
    let resp = post(
        &app,
        "/db/_all_docs?skip=1&limit=1",
        json!({"keys": ["a", "c", "zz"]}),
    )
    .await;
    assert_eq!(keys(&resp), [json!("c")]);
}

#[tokio::test]
async fn http_adapter_all_docs_against_server() {
    // The project's own HttpAdapter sends JSON-encoded keys.
    let (_db, app) = seeded().await;
    let addr = serve(app).await;
    let remote = Database::http(&format!("http://{addr}/{DB}"));

    let resp = remote
        .all_docs(rouchdb::AllDocsOptions {
            start_key: Some("b".into()),
            ..rouchdb::AllDocsOptions::new()
        })
        .await
        .unwrap();
    let got: Vec<_> = resp.rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(got, ["b", "c"]);

    let resp = remote
        .all_docs(rouchdb::AllDocsOptions {
            key: Some("a".into()),
            ..rouchdb::AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(resp.rows.len(), 1);
    assert_eq!(resp.rows[0].id.as_deref(), Some("a"));

    // Error rows survive the round trip through the server.
    let resp = remote
        .all_docs(rouchdb::AllDocsOptions {
            keys: Some(vec!["zz".into(), "a".into()]),
            ..rouchdb::AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(resp.rows[0], rouchdb::AllDocsRow::not_found("zz"));
    assert_eq!(resp.rows[1].id.as_deref(), Some("a"));
}
