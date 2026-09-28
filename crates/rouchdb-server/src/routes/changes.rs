use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use rouchdb_core::document::{ChangeEvent, ChangesOptions, ChangesResponse, ChangesStyle, Seq};
use rouchdb_core::error::RouchError;

use crate::error::AppError;
use crate::state::AppState;

/// CouchDB's default `timeout` for longpoll and continuous feeds.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// How often waiting feeds re-check the database when no write through the
/// server woke them up (e.g. writes made directly on the shared `Database`).
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Deserialize, Default)]
pub struct ChangesQuery {
    pub since: Option<String>,
    pub limit: Option<u64>,
    #[serde(default)]
    pub descending: Option<bool>,
    #[serde(default)]
    pub include_docs: Option<bool>,
    pub style: Option<String>,
    #[serde(default)]
    pub conflicts: Option<bool>,
    pub doc_ids: Option<String>,
    pub filter: Option<String>,
    pub feed: Option<String>,
    pub timeout: Option<u64>,
    pub heartbeat: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Feed {
    Normal,
    Longpoll,
    Continuous,
    EventSource,
}

/// A validated `_changes` request.
struct ChangesRequest {
    opts: ChangesOptions,
    /// `filter=_design`: only design documents.
    design_only: bool,
    feed: Feed,
    /// Inactivity timeout of waiting feeds; `None` (heartbeat set) waits forever.
    timeout: Option<Duration>,
    heartbeat: Option<Duration>,
}

fn bad_request(reason: &str) -> AppError {
    AppError(RouchError::BadRequest(reason.to_string()))
}

/// Resolve the `since` query value to a concrete sequence.
///
/// CouchDB's `since=now` means "the database's current update_seq", so resolve
/// it against `info()` rather than fabricating `u64::MAX` (which would overflow
/// the adapters' `since + 1` arithmetic).
async fn resolve_since(state: &AppState, since: Option<String>) -> Result<Seq, AppError> {
    match since {
        None => Ok(Seq::from(0u64)),
        Some(s) => {
            if s == "now" {
                Ok(state.db.info().await?.update_seq)
            } else if let Ok(n) = s.parse::<u64>() {
                Ok(Seq::from(n))
            } else {
                // CouchDB string seqs — pass through
                Ok(Seq::Str(s))
            }
        }
    }
}

/// Parse `doc_ids` given as a JSON array (query string) or array value (body).
fn doc_id_list(value: &serde_json::Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(String::from))
        .collect()
}

/// Build the request from the query string and, for POST, the body. Body
/// parameters other than `doc_ids` / `selector` are accepted as a fallback
/// for the query string.
async fn parse_request(
    state: &AppState,
    query: ChangesQuery,
    body: Option<&serde_json::Value>,
) -> Result<ChangesRequest, AppError> {
    let from_body = |key: &str| body.and_then(|b| b.get(key));

    let feed = match query.feed.as_deref().unwrap_or("normal") {
        "normal" => Feed::Normal,
        "longpoll" => Feed::Longpoll,
        "continuous" | "live" => Feed::Continuous,
        "eventsource" => Feed::EventSource,
        _ => {
            return Err(bad_request(
                "Supported `feed` types: normal, continuous, live, longpoll, eventsource",
            ));
        }
    };

    let style = match query
        .style
        .as_deref()
        .or_else(|| from_body("style").and_then(|v| v.as_str()))
    {
        Some("all_docs") => ChangesStyle::AllDocs,
        _ => ChangesStyle::MainOnly,
    };

    // `since` may be a number or a string, in the query or the body.
    let since = query.since.or_else(|| match from_body("since") {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    });

    let body_doc_ids = from_body("doc_ids").and_then(doc_id_list);
    let body_selector = from_body("selector").cloned();
    let mut design_only = false;
    let (doc_ids, selector) = match query.filter.as_deref() {
        // Without a filter, POST bodies may still restrict by doc_ids/selector.
        None => (body_doc_ids, body_selector),
        Some("_doc_ids") => {
            let from_query = match query.doc_ids.as_deref() {
                Some(raw) => serde_json::from_str::<serde_json::Value>(raw)
                    .ok()
                    .and_then(|v| doc_id_list(&v)),
                None => None,
            };
            let ids = from_query.or(body_doc_ids).ok_or_else(|| {
                bad_request("`doc_ids` filter parameter is not a list of doc ids.")
            })?;
            (Some(ids), None)
        }
        Some("_selector") => {
            let selector = body_selector
                .ok_or_else(|| bad_request("Selector must be specified in POST payload"))?;
            (None, Some(selector))
        }
        Some("_design") => {
            design_only = true;
            (None, None)
        }
        Some("_view") => {
            return Err(bad_request(
                "The `_view` filter is not supported: views are not executed server-side",
            ));
        }
        Some(filter) => {
            // `ddoc/name` filters are JavaScript functions, which RouchDB
            // cannot run. Refuse them instead of returning unfiltered changes.
            let Some((ddoc, _)) = filter.split_once('/') else {
                return Err(bad_request(
                    "`filter` must be of the form `designname/filtername`",
                ));
            };
            state
                .db
                .get(&format!("_design/{ddoc}"))
                .await
                .map_err(|e| match e {
                    RouchError::NotFound(_) => AppError(RouchError::NotFound("missing".into())),
                    e => AppError(e),
                })?;
            return Err(bad_request("JavaScript filter functions are not supported"));
        }
    };

    let heartbeat = match query.heartbeat.as_deref() {
        None | Some("false") => None,
        Some("true") => Some(Duration::from_millis(DEFAULT_TIMEOUT_MS)),
        Some(ms) => Some(Duration::from_millis(
            ms.parse::<u64>()
                .ok()
                .filter(|ms| *ms > 0)
                .ok_or_else(|| bad_request("Invalid heartbeat value"))?,
        )),
    };
    // As in CouchDB, a heartbeat keeps the feed open indefinitely.
    let timeout = match heartbeat {
        Some(_) => None,
        None => Some(Duration::from_millis(
            query.timeout.unwrap_or(DEFAULT_TIMEOUT_MS),
        )),
    };

    let opts = ChangesOptions {
        since: resolve_since(state, since).await?,
        limit: query
            .limit
            .or_else(|| from_body("limit").and_then(|v| v.as_u64())),
        descending: feed == Feed::Normal
            && query
                .descending
                .or_else(|| from_body("descending").and_then(|v| v.as_bool()))
                .unwrap_or(false),
        include_docs: query
            .include_docs
            .or_else(|| from_body("include_docs").and_then(|v| v.as_bool()))
            .unwrap_or(false),
        live: false,
        doc_ids,
        selector,
        conflicts: query
            .conflicts
            .or_else(|| from_body("conflicts").and_then(|v| v.as_bool()))
            .unwrap_or(false),
        style,
    };

    Ok(ChangesRequest {
        opts,
        design_only,
        feed,
        timeout,
        heartbeat,
    })
}

/// Run one changes query, applying the `_design` filter.
async fn fetch(
    state: &AppState,
    opts: &ChangesOptions,
    design_only: bool,
) -> Result<ChangesResponse, RouchError> {
    if !design_only {
        return state.db.changes(opts.clone()).await;
    }
    // Filter before limiting, so `limit` counts design documents only.
    let mut all = opts.clone();
    all.limit = None;
    let mut response = state.db.changes(all).await?;
    response.results.retain(|c| c.id.starts_with("_design/"));
    if let Some(limit) = opts.limit
        && response.results.len() > limit as usize
    {
        response.results.truncate(limit as usize);
        response.last_seq = response.results.last().unwrap().seq.clone();
    }
    Ok(response)
}

fn normal_body(results: &[ChangeEvent], last_seq: &Seq) -> serde_json::Value {
    serde_json::json!({
        "results": results,
        "last_seq": last_seq,
        "pending": 0,
    })
}

async fn changes_response(state: AppState, req: ChangesRequest) -> Result<Response, AppError> {
    // Subscribe before the first query so a write racing with it still
    // wakes the feed up.
    let writes = state.writes.subscribe();
    let first = fetch(&state, &req.opts, req.design_only).await?;

    let longpoll_ready = req.feed == Feed::Longpoll && !first.results.is_empty();
    if req.feed == Feed::Normal || longpoll_ready {
        return Ok(Json(normal_body(&first.results, &first.last_seq)).into_response());
    }

    let content_type = if req.feed == Feed::EventSource {
        "text/event-stream"
    } else {
        "application/json"
    };
    let (tx, rx) = mpsc::channel::<Bytes>(16);
    tokio::spawn(run_feed(state, req, first, writes, tx));

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<_, Infallible>(chunk), rx))
    });
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type)],
        Body::from_stream(stream),
    )
        .into_response())
}

/// Encode one change for a streaming feed.
fn encode_change(feed: Feed, change: &ChangeEvent) -> Bytes {
    let json = serde_json::to_string(change).unwrap();
    match feed {
        Feed::EventSource => {
            let id = serde_json::to_string(&change.seq).unwrap();
            Bytes::from(format!("data: {json}\nid: {id}\n\n"))
        }
        _ => Bytes::from(format!("{json}\n")),
    }
}

/// Drive a longpoll / continuous / eventsource feed until it ends (timeout,
/// limit, a longpoll result) or the client goes away.
async fn run_feed(
    state: AppState,
    mut req: ChangesRequest,
    first: ChangesResponse,
    mut writes: watch::Receiver<u64>,
    tx: mpsc::Sender<Bytes>,
) {
    let feed = req.feed;
    let mut remaining = req.opts.limit;
    let mut batch = first;
    let mut heartbeat = req
        .heartbeat
        .map(|hb| tokio::time::interval_at(Instant::now() + hb, hb));
    let mut deadline = req.timeout.map(|t| Instant::now() + t);

    loop {
        req.opts.since = batch.last_seq.clone();
        if feed == Feed::Longpoll {
            if !batch.results.is_empty() {
                let body = normal_body(&batch.results, &batch.last_seq);
                let _ = tx.send(Bytes::from(body.to_string())).await;
                return;
            }
        } else if !batch.results.is_empty() {
            for change in &batch.results {
                if tx.send(encode_change(feed, change)).await.is_err() {
                    return;
                }
            }
            if let Some(n) = remaining.as_mut() {
                *n = n.saturating_sub(batch.results.len() as u64);
                if *n == 0 {
                    break;
                }
            }
            // The continuous timeout measures inactivity.
            deadline = req.timeout.map(|t| Instant::now() + t);
        }

        // Wait for a write, the poll interval, a heartbeat or the deadline.
        loop {
            let beat = async {
                match heartbeat.as_mut() {
                    Some(interval) => {
                        interval.tick().await;
                    }
                    None => std::future::pending().await,
                }
            };
            let expire = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                // The client went away.
                _ = tx.closed() => return,
                _ = writes.changed() => break,
                _ = tokio::time::sleep(POLL_INTERVAL) => break,
                _ = beat => {
                    let beat = if feed == Feed::EventSource {
                        "event: heartbeat\ndata: \n\n"
                    } else {
                        "\n"
                    };
                    if tx.send(Bytes::from_static(beat.as_bytes())).await.is_err() {
                        return;
                    }
                }
                _ = expire => {
                    if feed == Feed::Longpoll {
                        let body = normal_body(&[], &req.opts.since);
                        let _ = tx.send(Bytes::from(body.to_string())).await;
                        return;
                    }
                    finish(feed, &req.opts.since, &tx).await;
                    return;
                }
            }
        }

        writes.borrow_and_update();
        // A deleted database ends the feed.
        if !state.db_exists() {
            if feed == Feed::Longpoll {
                let body = normal_body(&[], &req.opts.since);
                let _ = tx.send(Bytes::from(body.to_string())).await;
                return;
            }
            break;
        }
        let mut opts = req.opts.clone();
        opts.limit = remaining;
        batch = match fetch(&state, &opts, req.design_only).await {
            Ok(batch) => batch,
            Err(_) => break,
        };
    }

    finish(feed, &req.opts.since, &tx).await;
}

/// End a continuous feed with its `last_seq` line (eventsource has none).
async fn finish(feed: Feed, last_seq: &Seq, tx: &mpsc::Sender<Bytes>) {
    if feed == Feed::Continuous {
        let line = serde_json::json!({"last_seq": last_seq, "pending": 0});
        let _ = tx.send(Bytes::from(format!("{line}\n"))).await;
    }
}

/// Middleware: wake up waiting feeds after every request that may have
/// written to the database.
pub async fn notify_writes(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let may_write = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let response = next.run(req).await;
    if may_write && response.status().is_success() {
        state.writes.send_modify(|n| *n = n.wrapping_add(1));
    }
    response
}

/// GET /{db}/_changes — get the changes feed.
pub async fn get_changes(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Query(query): Query<ChangesQuery>,
) -> Result<Response, AppError> {
    state.check_db(&db)?;
    let req = parse_request(&state, query, None).await?;
    changes_response(state, req).await
}

/// POST /{db}/_changes — get the changes feed with body params.
pub async fn post_changes(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Query(query): Query<ChangesQuery>,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    state.check_db(&db)?;
    let req = parse_request(&state, query, Some(&body)).await?;
    changes_response(state, req).await
}
