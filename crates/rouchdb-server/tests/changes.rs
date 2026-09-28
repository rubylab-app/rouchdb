//! F62: `_changes` supports longpoll / continuous / eventsource feeds, the
//! built-in filters, `doc_ids` in GET, and a numeric `since` in POST.
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use rouchdb::Database;
use serde_json::json;
use tower::ServiceExt;

async fn seeded() -> (Arc<Database>, axum::Router) {
    let db = Arc::new(Database::memory(DB));
    for id in ["a", "b", "c", "_design/x"] {
        db.put(id, json!({"type": id})).await.unwrap();
    }
    let app = app_with(db.clone(), &config());
    (db, app)
}

fn ids(body: &serde_json::Value) -> Vec<String> {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

// ─── filters ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn doc_ids_filter_in_get() {
    let (_db, app) = seeded().await;
    let uri = format!("/db/_changes?filter=_doc_ids&doc_ids={}", q(json!(["b"])));
    let resp = get(&app, &uri).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(ids(&resp.json()), ["b"]);

    let resp = get(&app, "/db/_changes?filter=_doc_ids").await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);

    let resp = post(
        &app,
        "/db/_changes?filter=_doc_ids",
        json!({"doc_ids": ["a", "c"]}),
    )
    .await;
    assert_eq!(ids(&resp.json()), ["a", "c"]);
}

#[tokio::test]
async fn design_and_selector_filters() {
    let (_db, app) = seeded().await;
    let resp = get(&app, "/db/_changes?filter=_design").await;
    assert_eq!(ids(&resp.json()), ["_design/x"]);

    let resp = get(&app, "/db/_changes?filter=_selector").await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json()["reason"],
        "Selector must be specified in POST payload"
    );

    let resp = post(
        &app,
        "/db/_changes?filter=_selector",
        json!({"selector": {"type": "c"}}),
    )
    .await;
    assert_eq!(ids(&resp.json()), ["c"]);
}

#[tokio::test]
async fn unsupported_filters_are_rejected_instead_of_ignored() {
    let (db, app) = seeded().await;

    // A replication filtered by a design-doc filter must not silently
    // replicate everything.
    let resp = get(&app, "/db/_changes?filter=app/by_type").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    db.put(
        "_design/app",
        json!({"filters": {"by_type": "function(doc) { return true; }"}}),
    )
    .await
    .unwrap();
    let resp = get(&app, "/db/_changes?filter=app/by_type").await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.json()["error"], "bad_request");

    assert_eq!(
        get(&app, "/db/_changes?filter=_view").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&app, "/db/_changes?filter=bogus").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&app, "/db/_changes?feed=bogus").await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn post_accepts_a_numeric_since() {
    let (_db, app) = seeded().await;
    let resp = post(&app, "/db/_changes", json!({"since": 2})).await;
    assert_eq!(ids(&resp.json()), ["c", "_design/x"]);
    let resp = post(&app, "/db/_changes", json!({"since": "2"})).await;
    assert_eq!(ids(&resp.json()), ["c", "_design/x"]);
}

// ─── longpoll ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn longpoll_returns_at_once_when_changes_exist() {
    let (_db, app) = seeded().await;
    let started = Instant::now();
    let resp = get(&app, "/db/_changes?feed=longpoll&since=1&timeout=10000").await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(ids(&resp.json()), ["b", "c", "_design/x"]);
}

#[tokio::test]
async fn longpoll_waits_for_the_next_change() {
    let (_db, app) = seeded().await;
    let request = app.clone().oneshot(
        Request::get("/db/_changes?feed=longpoll&since=now&timeout=10000")
            .body(Body::empty())
            .unwrap(),
    );
    let pending = tokio::spawn(async move {
        let resp = request.await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!pending.is_finished(), "longpoll must wait for a change");
    let started = Instant::now();
    put(&app, "/db/new", json!({})).await;

    let body = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("longpoll did not return after a write")
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(ids(&body), ["new"]);
    assert_eq!(body["last_seq"], 5);
}

#[tokio::test]
async fn longpoll_sees_writes_made_outside_the_server() {
    let (db, app) = seeded().await;
    let pending = tokio::spawn(async move {
        get(&app, "/db/_changes?feed=longpoll&since=now&timeout=10000")
            .await
            .json()
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    db.put("direct", json!({})).await.unwrap();
    let body = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("longpoll did not notice the write")
        .unwrap();
    assert_eq!(ids(&body), ["direct"]);
}

#[tokio::test]
async fn longpoll_times_out_with_no_results() {
    let (_db, app) = seeded().await;
    let started = Instant::now();
    let resp = get(&app, "/db/_changes?feed=longpoll&since=now&timeout=300").await;
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["results"], json!([]));
    assert_eq!(body["last_seq"], 4);
}

// ─── continuous / eventsource ───────────────────────────────────────────────

fn lines(body: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(body)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn continuous_streams_one_change_per_line() {
    let (_db, app) = seeded().await;
    let app2 = app.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        put(&app2, "/db/later", json!({})).await;
    });

    let resp = get(&app, "/db/_changes?feed=continuous&since=2&timeout=600").await;
    writer.await.unwrap();
    assert_eq!(resp.status, StatusCode::OK);
    let rows = lines(&resp.body);
    let got: Vec<_> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(got, ["c", "_design/x", "later"]);
    let last = rows.last().unwrap();
    assert_eq!(last["last_seq"], 5);
}

#[tokio::test]
async fn continuous_stops_at_limit() {
    let (_db, app) = seeded().await;
    let started = Instant::now();
    let resp = get(&app, "/db/_changes?feed=continuous&limit=2&timeout=10000").await;
    assert!(started.elapsed() < Duration::from_secs(2));
    let rows = lines(&resp.body);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], "a");
    assert_eq!(rows[1]["id"], "b");
    assert_eq!(rows[2]["last_seq"], 2);
}

#[tokio::test]
async fn continuous_sends_heartbeats() {
    let (_db, app) = seeded().await;
    let resp = app
        .clone()
        .oneshot(
            Request::get("/db/_changes?feed=continuous&since=now&heartbeat=50")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = resp.into_body();
    let mut newlines = 0;
    while newlines < 3 {
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("no heartbeat")
            .unwrap()
            .unwrap();
        if let Ok(data) = frame.into_data() {
            assert!(data.iter().all(|b| *b == b'\n'), "{data:?}");
            newlines += data.len();
        }
    }
}

#[tokio::test]
async fn eventsource_feed() {
    let (_db, app) = seeded().await;
    let resp = get(&app, "/db/_changes?feed=eventsource&limit=2&timeout=100").await;
    assert_eq!(resp.header("content-type"), Some("text/event-stream"));
    let text = String::from_utf8(resp.body.to_vec()).unwrap();
    let events: Vec<&str> = text
        .split("\n\n")
        .filter(|e| !e.trim().is_empty())
        .collect();
    assert_eq!(events.len(), 2);
    assert!(events[0].starts_with("data: {"));
    assert!(events[0].contains("\"id\":\"a\""));
    assert!(events[0].contains("\nid: 1"));
}
