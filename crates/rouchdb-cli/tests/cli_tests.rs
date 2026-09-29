use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use assert_cmd::Command;
use base64::Engine;
use predicates::prelude::*;
use tempfile::TempDir;

// CouchDB settings and the guard that deletes test databases, shared with
// the rouchdb integration tests.
#[path = "../../rouchdb/tests/common/mod.rs"]
mod common;

async fn setup_db(docs: &[(&str, serde_json::Value)]) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.redb");
    {
        let db = rouchdb::Database::open(&db_path, "test").unwrap();
        for (id, data) in docs {
            db.put(id, data.clone()).await.unwrap();
        }
        // db dropped here — releases redb file lock
    }
    (dir, db_path)
}

/// Create a database with `n` padded documents in a single transaction.
async fn setup_bulk_db(n: usize, padding: usize) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.redb");
    {
        let db = rouchdb::Database::open(&db_path, "test").unwrap();
        let docs = (0..n)
            .map(|i| rouchdb::Document {
                id: format!("doc{:05}", i),
                rev: None,
                deleted: false,
                data: serde_json::json!({"i": i, "pad": "x".repeat(padding)}),
                attachments: HashMap::new(),
            })
            .collect();
        let results = db
            .bulk_docs(docs, rouchdb::BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(results.iter().all(|r| r.ok));
    }
    (dir, db_path)
}

#[allow(deprecated)]
fn rouchdb_cmd() -> Command {
    Command::cargo_bin("rouchdb").unwrap()
}

fn run(args: &[&str]) -> Output {
    rouchdb_cmd().args(args).output().unwrap()
}

fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({}): {:?} / stderr: {}",
            e,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn stderr_str(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ─── FAKE COUCHDB ───────────────────────────────────────────────────────────

/// Server uuid the fake CouchDB reports in its welcome message (`GET /`).
const FAKE_UUID: &str = "0f7ab6e3c1d24c8e9a5b3d2e1f0c9b8a";

/// A request captured by the fake CouchDB server.
#[derive(Debug, Clone)]
struct FakeRequest {
    method: String,
    /// Path and query string, as sent.
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl FakeRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The path without the query string.
    fn route(&self) -> &str {
        self.path.split('?').next().unwrap_or_default()
    }

    /// The raw value of query parameter `name`.
    fn query(&self, name: &str) -> Option<&str> {
        let (_, query) = self.path.split_once('?')?;
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then_some(value)
        })
    }

    fn json(&self) -> Result<serde_json::Value, String> {
        serde_json::from_slice(&self.body)
            .map_err(|e| format!("{} {}: body is not JSON: {}", self.method, self.path, e))
    }
}

fn read_request(stream: &TcpStream) -> Option<FakeRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;

    Some(FakeRequest {
        method,
        path,
        headers,
        body,
    })
}

/// How the fake CouchDB answers one request.
enum FakeReply {
    Json(u16, serde_json::Value),
    /// Close the connection without answering, like a network failure.
    HangUp,
}

/// Behaviour switches of the fake CouchDB.
#[derive(Debug, Default, Clone, Copy)]
struct FakeConfig {
    /// `_bulk_docs` rejects every document the way a `validate_doc_update`
    /// function does (per-doc `forbidden`, HTTP 201).
    reject_writes: bool,
    /// Hang up on `PUT /db/_local/...`, so every checkpoint write fails.
    hang_up_on_checkpoint: bool,
}

/// State of the fake CouchDB, which serves one database named `db`.
#[derive(Default)]
struct FakeState {
    /// Every request received, in order.
    requests: Vec<FakeRequest>,
    /// Protocol violations and handler failures, answered with a 500. Tests
    /// assert this stays empty, so a bad request fails the test with a
    /// message rather than with a panic in the server thread (which the CLI
    /// would only see as a reset connection).
    problems: Vec<String>,
    /// `_local` documents by id (without the `_local/` prefix), with `_rev`.
    local: BTreeMap<String, serde_json::Value>,
    /// Stored revisions in write order, each a full document with `_id`,
    /// `_rev` and `_revisions`. A document's seq is the (1-based) position
    /// of its last stored revision.
    revs: Vec<serde_json::Value>,
}

fn doc_id(doc: &serde_json::Value) -> &str {
    doc["_id"].as_str().unwrap_or_default()
}

fn doc_rev(doc: &serde_json::Value) -> &str {
    doc["_rev"].as_str().unwrap_or_default()
}

fn is_deleted(doc: &serde_json::Value) -> bool {
    doc["_deleted"] == true
}

/// `(pos, hash)` of a `pos-hash` revision.
fn parse_rev(rev: &str) -> Option<(u64, &str)> {
    let (pos, hash) = rev.split_once('-')?;
    Some((pos.parse().ok()?, hash))
}

/// The revisions `doc` descends from, per its `_revisions`.
fn ancestor_revs(doc: &serde_json::Value) -> Vec<String> {
    let start = doc["_revisions"]["start"].as_u64().unwrap_or(0);
    let ids = doc["_revisions"]["ids"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    ids.iter()
        .enumerate()
        .skip(1)
        .filter_map(|(i, hash)| {
            Some(format!(
                "{}-{}",
                start.checked_sub(i as u64)?,
                hash.as_str()?
            ))
        })
        .collect()
}

impl FakeState {
    fn revs_of(&self, id: &str) -> Vec<&serde_json::Value> {
        self.revs.iter().filter(|d| doc_id(d) == id).collect()
    }

    /// Leaf revisions of `id`, winner first: live before deleted, then the
    /// highest revision (CouchDB's order).
    fn leaves(&self, id: &str) -> Vec<&serde_json::Value> {
        let revs = self.revs_of(id);
        let inner: HashSet<String> = revs.iter().flat_map(|d| ancestor_revs(d)).collect();
        let mut leaves: Vec<&serde_json::Value> = revs
            .into_iter()
            .filter(|d| !inner.contains(doc_rev(d)))
            .collect();
        let key = |d: &serde_json::Value| {
            (
                !is_deleted(d),
                parse_rev(doc_rev(d)).map(|(pos, hash)| (pos, hash.to_string())),
            )
        };
        leaves.sort_by_key(|d| std::cmp::Reverse(key(d)));
        leaves
    }

    /// `(seq, id)` of every document, in seq order.
    fn feed(&self) -> Vec<(u64, String)> {
        let mut last_seq = BTreeMap::new();
        for (i, doc) in self.revs.iter().enumerate() {
            last_seq.insert(doc_id(doc).to_string(), i as u64 + 1);
        }
        let mut feed: Vec<(u64, String)> =
            last_seq.into_iter().map(|(id, seq)| (seq, id)).collect();
        feed.sort();
        feed
    }
}

/// Handle on a running fake CouchDB.
struct FakeCouch {
    base: String,
    state: Arc<Mutex<FakeState>>,
}

impl FakeCouch {
    /// URL of the database the fake serves.
    fn db_url(&self) -> String {
        format!("{}/db", self.base)
    }

    /// `db_url` with `userinfo` (`user:password`) in its authority.
    fn db_url_with_userinfo(&self, userinfo: &str) -> String {
        self.db_url()
            .replacen("http://", &format!("http://{}@", userinfo), 1)
    }

    fn state(&self) -> MutexGuard<'_, FakeState> {
        lock(&self.state)
    }

    fn requests(&self) -> Vec<FakeRequest> {
        self.state().requests.clone()
    }

    /// Stored revisions ordered by document id then revision, as the
    /// replicator may write a batch in any order.
    fn stored_revs(&self) -> Vec<serde_json::Value> {
        let mut revs = self.state().revs.clone();
        revs.sort_by(|a, b| (doc_id(a), doc_rev(a)).cmp(&(doc_id(b), doc_rev(b))));
        revs
    }

    fn assert_no_problems(&self) {
        let problems = self.state().problems.clone();
        assert!(
            problems.is_empty(),
            "fake CouchDB rejected requests: {:#?}",
            problems
        );
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A revision to seed the fake CouchDB with: `rev` is `pos-hash` and
/// `ancestors` are the hashes of revisions `pos-1`, `pos-2`, ... (newest
/// first), as in `_revisions.ids`.
fn fake_rev(id: &str, rev: &str, ancestors: &[&str], body: serde_json::Value) -> serde_json::Value {
    let (pos, hash) = parse_rev(rev).expect("rev is pos-hash");
    let mut ids = vec![hash];
    ids.extend_from_slice(ancestors);
    let mut doc = body;
    let obj = doc.as_object_mut().expect("body is an object");
    obj.insert("_id".into(), id.into());
    obj.insert("_rev".into(), rev.into());
    obj.insert(
        "_revisions".into(),
        serde_json::json!({"start": pos, "ids": ids}),
    );
    doc
}

/// Serve a CouchDB stand-in holding the database `db` (seeded with `revs`)
/// on an ephemeral port, one request per connection.
fn spawn_fake_couchdb(config: FakeConfig, revs: Vec<serde_json::Value>) -> FakeCouch {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake CouchDB");
    let port = listener.local_addr().expect("fake CouchDB address").port();
    let state = Arc::new(Mutex::new(FakeState {
        revs,
        ..Default::default()
    }));
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(req) = read_request(&stream) else {
                continue;
            };
            let reply = {
                let mut state = lock(&shared);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle_fake_request(config, &mut state, &req)
                }));
                let reply = match outcome {
                    Ok(Ok(reply)) => reply,
                    Ok(Err(problem)) => {
                        state.problems.push(problem.clone());
                        FakeReply::Json(
                            500,
                            serde_json::json!({"error": "fake", "reason": problem}),
                        )
                    }
                    Err(_) => {
                        let problem = format!("handler panicked on {} {}", req.method, req.path);
                        state.problems.push(problem.clone());
                        FakeReply::Json(
                            500,
                            serde_json::json!({"error": "fake", "reason": problem}),
                        )
                    }
                };
                state.requests.push(req);
                reply
            };
            match reply {
                FakeReply::Json(status, body) => {
                    let body = body.to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status,
                        body.len(),
                        body
                    );
                }
                FakeReply::HangUp => drop(stream),
            }
        }
    });
    FakeCouch {
        base: format!("http://127.0.0.1:{}", port),
        state,
    }
}

fn not_found() -> FakeReply {
    FakeReply::Json(
        404,
        serde_json::json!({"error": "not_found", "reason": "missing"}),
    )
}

fn handle_fake_request(
    config: FakeConfig,
    state: &mut FakeState,
    req: &FakeRequest,
) -> Result<FakeReply, String> {
    let method = req.method.as_str();
    if let Some(id) = req.route().strip_prefix("/db/_local/") {
        return handle_fake_local(config, state, req, id);
    }
    match (method, req.route()) {
        ("GET", "/") => Ok(FakeReply::Json(
            200,
            serde_json::json!({
                "couchdb": "Welcome",
                "version": "3.5.1",
                "git_sha": "44f6a43d8",
                "uuid": FAKE_UUID,
                "features": ["access-ready", "partitioned", "pluggable-storage-engines", "reshard", "scheduler"],
                "vendor": {"name": "The Apache Software Foundation"}
            }),
        )),
        ("GET", "/db") => {
            let feed = state.feed();
            let deleted = feed
                .iter()
                .filter(|(_, id)| state.leaves(id).first().is_some_and(|d| is_deleted(d)))
                .count();
            Ok(FakeReply::Json(
                200,
                serde_json::json!({
                    "db_name": "db",
                    "doc_count": feed.len() - deleted,
                    "doc_del_count": deleted,
                    "update_seq": state.revs.len(),
                    "purge_seq": 0,
                    "instance_start_time": "0"
                }),
            ))
        }
        ("POST", "/db/_revs_diff") => fake_revs_diff(state, &req.json()?),
        ("POST", "/db/_bulk_docs") => fake_bulk_docs(config, state, &req.json()?),
        ("GET", "/db/_changes") => fake_changes(state, req),
        ("POST", "/db/_bulk_get") => fake_bulk_get(state, req),
        _ => Err(format!("unexpected request {} {}", method, req.path)),
    }
}

/// `_local` documents, with CouchDB's `0-N` revisions. Stricter than
/// CouchDB 3.5, which accepts any well-formed `0-N` `_rev` (and a missing
/// one) on update: here an update must carry the current `_rev` or it is a
/// conflict, so a replicator that drops or garbles the rev it read is caught.
fn handle_fake_local(
    config: FakeConfig,
    state: &mut FakeState,
    req: &FakeRequest,
    id: &str,
) -> Result<FakeReply, String> {
    match req.method.as_str() {
        "GET" => Ok(match state.local.get(id) {
            Some(doc) => FakeReply::Json(200, doc.clone()),
            None => not_found(),
        }),
        "PUT" if config.hang_up_on_checkpoint => Ok(FakeReply::HangUp),
        "PUT" => {
            let mut doc = req.json()?;
            let current = state.local.get(id).map(|d| doc_rev(d).to_string());
            let sent = doc["_rev"].as_str().map(String::from);
            if sent != current {
                return Ok(FakeReply::Json(
                    409,
                    serde_json::json!({"error": "conflict", "reason": "Document update conflict."}),
                ));
            }
            let n: u64 = current
                .as_deref()
                .and_then(|rev| rev.strip_prefix("0-")?.parse().ok())
                .unwrap_or(0);
            let rev = format!("0-{}", n + 1);
            let obj = doc
                .as_object_mut()
                .ok_or_else(|| format!("PUT _local/{}: body is not an object", id))?;
            obj.insert("_id".into(), format!("_local/{}", id).into());
            obj.insert("_rev".into(), rev.clone().into());
            state.local.insert(id.to_string(), doc);
            Ok(FakeReply::Json(
                201,
                serde_json::json!({"ok": true, "id": format!("_local/{}", id), "rev": rev}),
            ))
        }
        _ => Err(format!("unexpected request {} {}", req.method, req.path)),
    }
}

/// `_revs_diff`: the revisions not stored yet, with the stored leaves they
/// may descend from as `possible_ancestors`.
fn fake_revs_diff(state: &FakeState, body: &serde_json::Value) -> Result<FakeReply, String> {
    let request = body
        .as_object()
        .ok_or_else(|| format!("_revs_diff body is not an object: {}", body))?;
    let mut response = serde_json::Map::new();
    for (id, revs) in request {
        let revs = revs
            .as_array()
            .and_then(|revs| revs.iter().map(|r| r.as_str()).collect::<Option<Vec<_>>>())
            .ok_or_else(|| format!("_revs_diff: revs of {} are not strings: {}", id, revs))?;
        let known: HashSet<&str> = state.revs_of(id).into_iter().map(doc_rev).collect();
        let missing: Vec<&str> = revs.into_iter().filter(|r| !known.contains(r)).collect();
        let Some(newest) = missing
            .iter()
            .filter_map(|r| parse_rev(r))
            .map(|(pos, _)| pos)
            .max()
        else {
            continue;
        };
        let mut entry = serde_json::json!({ "missing": missing });
        let ancestors: Vec<&str> = state
            .leaves(id)
            .into_iter()
            .map(doc_rev)
            .filter(|r| parse_rev(r).is_some_and(|(pos, _)| pos < newest))
            .collect();
        if !ancestors.is_empty() {
            entry["possible_ancestors"] = serde_json::json!(ancestors);
        }
        response.insert(id.clone(), entry);
    }
    Ok(FakeReply::Json(200, serde_json::Value::Object(response)))
}

/// `_bulk_docs` as a replicator must use it: `new_edits: false`, and every
/// document with its `_rev` and a `_revisions` history ending in it.
fn fake_bulk_docs(
    config: FakeConfig,
    state: &mut FakeState,
    body: &serde_json::Value,
) -> Result<FakeReply, String> {
    if body["new_edits"] != false {
        return Err(format!("_bulk_docs without new_edits:false: {}", body));
    }
    let docs = body["docs"]
        .as_array()
        .ok_or_else(|| format!("_bulk_docs body has no docs: {}", body))?;
    for doc in docs {
        let (pos, hash) = parse_rev(doc_rev(doc))
            .filter(|_| !doc_id(doc).is_empty())
            .ok_or_else(|| format!("_bulk_docs: doc without _id or _rev: {}", doc))?;
        let revisions = &doc["_revisions"];
        if revisions["start"] != pos || revisions["ids"][0] != hash {
            return Err(format!(
                "_bulk_docs: _revisions of {} {} do not end in that rev: {}",
                doc_id(doc),
                doc_rev(doc),
                revisions
            ));
        }
    }
    if config.reject_writes {
        let results = docs
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d["_id"], "rev": d["_rev"],
                    "error": "forbidden", "reason": "rejected by validator"
                })
            })
            .collect();
        return Ok(FakeReply::Json(201, serde_json::Value::Array(results)));
    }
    for doc in docs {
        let stored = state
            .revs
            .iter()
            .any(|d| doc_id(d) == doc_id(doc) && doc_rev(d) == doc_rev(doc));
        if !stored {
            state.revs.push(doc.clone());
        }
    }
    // With new_edits:false CouchDB lists only the failures: [] means every
    // document was stored.
    Ok(FakeReply::Json(201, serde_json::json!([])))
}

/// `_changes`, which a replicator must read with `style=all_docs` so that
/// conflicting leaves are listed too.
fn fake_changes(state: &FakeState, req: &FakeRequest) -> Result<FakeReply, String> {
    if req.query("style") != Some("all_docs") {
        return Err(format!("_changes without style=all_docs: {}", req.path));
    }
    let since: u64 = req
        .query("since")
        .unwrap_or("0")
        .parse()
        .map_err(|e| format!("_changes since: {}: {}", req.path, e))?;
    let limit: usize = match req.query("limit") {
        Some(limit) => limit
            .parse()
            .map_err(|e| format!("_changes limit: {}: {}", req.path, e))?,
        None => usize::MAX,
    };
    let results: Vec<serde_json::Value> = state
        .feed()
        .into_iter()
        .filter(|(seq, _)| *seq > since)
        .take(limit)
        .map(|(seq, id)| {
            let leaves = state.leaves(&id);
            let changes: Vec<serde_json::Value> = leaves
                .iter()
                .map(|d| serde_json::json!({"rev": doc_rev(d)}))
                .collect();
            let mut row = serde_json::json!({"seq": seq, "id": id, "changes": changes});
            if leaves.first().is_some_and(|d| is_deleted(d)) {
                row["deleted"] = true.into();
            }
            row
        })
        .collect();
    let last_seq = results
        .last()
        .map(|row| row["seq"].clone())
        .unwrap_or_else(|| since.max(state.revs.len() as u64).into());
    Ok(FakeReply::Json(
        200,
        serde_json::json!({"results": results, "last_seq": last_seq, "pending": 0}),
    ))
}

/// `_bulk_get`, which a replicator must call with `revs=true` (to get the
/// `_revisions` history), `attachments=true` and `latest=true`.
fn fake_bulk_get(state: &FakeState, req: &FakeRequest) -> Result<FakeReply, String> {
    for param in ["revs", "attachments", "latest"] {
        if req.query(param) != Some("true") {
            return Err(format!("_bulk_get without {}=true: {}", param, req.path));
        }
    }
    let body = req.json()?;
    let items = body["docs"]
        .as_array()
        .ok_or_else(|| format!("_bulk_get body has no docs: {}", body))?;
    let mut results = Vec::new();
    for item in items {
        let id = item["id"]
            .as_str()
            .ok_or_else(|| format!("_bulk_get item without id: {}", item))?;
        let found = match item["rev"].as_str() {
            Some(rev) => state
                .revs
                .iter()
                .find(|d| doc_id(d) == id && doc_rev(d) == rev),
            None => state.leaves(id).first().copied(),
        };
        results.push(match found {
            Some(doc) => serde_json::json!({"id": id, "docs": [{"ok": doc}]}),
            None => serde_json::json!({"id": id, "docs": [{"error": {
                "id": id, "rev": item["rev"], "error": "not_found", "reason": "missing"
            }}]}),
        });
    }
    Ok(FakeReply::Json(
        200,
        serde_json::json!({ "results": results }),
    ))
}

/// A local URL whose port nothing is listening on.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Minimal raw HTTP client for setting up the real CouchDB in ignored tests.
fn couch_request(method: &str, url: &str, body: Option<&str>) -> (u16, String) {
    let rest = url.strip_prefix("http://").expect("http URL");
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let (userinfo, host) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let mut stream = TcpStream::connect(host).unwrap();
    let body = body.unwrap_or("");
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        method,
        path,
        host,
        body.len()
    );
    if let Some(userinfo) = userinfo {
        req.push_str(&format!(
            "Authorization: Basic {}\r\n",
            b64(userinfo.as_bytes())
        ));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

// ─── INFO ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn info_shows_doc_count() {
    let (_dir, db_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
        ("c", serde_json::json!({"x": 3})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args(["info", db_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["doc_count"], 3);
    assert_eq!(v["db_name"], "test");
}

#[tokio::test]
async fn info_empty_database() {
    let (_dir, db_path) = setup_db(&[]).await;

    let output = rouchdb_cmd()
        .args(["info", db_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["doc_count"], 0);
}

#[tokio::test]
async fn info_db_name_defaults_to_file_stem_and_can_be_overridden() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inventory.redb");
    let p = path_str(&path);
    assert!(run(&["put", p, "a", "{}"]).status.success());

    assert_eq!(stdout_json(&run(&["info", p]))["db_name"], "inventory");
    let output = run(&["info", p, "--db-name", "warehouse"]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["db_name"], "warehouse");
    assert_eq!(v["doc_count"], 1);
}

// ─── GET ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_existing_document() {
    let (_dir, db_path) =
        setup_db(&[("doc1", serde_json::json!({"name": "Alice", "age": 30}))]).await;

    let output = rouchdb_cmd()
        .args(["get", db_path.to_str().unwrap(), "doc1"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["_id"], "doc1");
    assert!(v["_rev"].as_str().unwrap().starts_with("1-"));
    assert_eq!(v["name"], "Alice");
    assert_eq!(v["age"], 30);
}

#[tokio::test]
async fn get_nonexistent_document_fails() {
    let (_dir, db_path) = setup_db(&[("doc1", serde_json::json!({"x": 1}))]).await;

    rouchdb_cmd()
        .args(["get", db_path.to_str().unwrap(), "no_such_doc"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Error"));
}

#[tokio::test]
async fn get_with_pretty_flag() {
    let (_dir, db_path) = setup_db(&[("doc1", serde_json::json!({"name": "Alice"}))]).await;

    let output = rouchdb_cmd()
        .args(["--pretty", "get", db_path.to_str().unwrap(), "doc1"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Pretty-printed JSON has newlines with indentation
    assert!(stdout.contains("\n  "));
}

#[tokio::test]
async fn get_with_specific_rev() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.redb");
    let rev1;
    {
        let db = rouchdb::Database::open(&db_path, "test").unwrap();
        let r1 = db
            .put("doc1", serde_json::json!({"version": 1}))
            .await
            .unwrap();
        rev1 = r1.rev.unwrap();
        assert!(
            db.update("doc1", &rev1, serde_json::json!({"version": 2}))
                .await
                .unwrap()
                .ok
        );
    }

    let output = rouchdb_cmd()
        .args(["get", db_path.to_str().unwrap(), "doc1", "--rev", &rev1])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["_rev"], rev1);
    assert_eq!(v["version"], 1);
}

#[tokio::test]
async fn get_conflicts_lists_losing_leaves_only_with_flag() {
    let (_dir, db_path) = setup_db(&[]).await;
    let [a, b, c] = ['a', 'b', 'c'].map(|h| format!("1-{}", h.to_string().repeat(32)));
    {
        // Three conflicting leaves, written the way replication does.
        let db = rouchdb::Database::open(&db_path, "test").unwrap();
        let leaves = [(&a, "a"), (&c, "c"), (&b, "b")]
            .into_iter()
            .map(|(rev, v)| rouchdb::Document {
                id: "doc".into(),
                rev: Some(rev.parse().unwrap()),
                deleted: false,
                data: serde_json::json!({ "v": v }),
                attachments: HashMap::new(),
            })
            .collect();
        let results = db
            .bulk_docs(leaves, rouchdb::BulkDocsOptions::replication())
            .await
            .unwrap();
        assert!(results.iter().all(|r| r.ok), "{:?}", results);
    }
    let p = path_str(&db_path);

    // CouchDB 3.5.1 answers exactly this: the highest rev wins and the
    // other leaves follow, highest first.
    let output = run(&["get", p, "doc", "--conflicts"]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({"_id": "doc", "_rev": c, "v": "c", "_conflicts": [b, a]})
    );
    assert_eq!(
        stdout_json(&run(&["get", p, "doc"])),
        serde_json::json!({"_id": "doc", "_rev": c, "v": "c"})
    );
}

// ─── ALL-DOCS ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn all_docs_lists_all() {
    let (_dir, db_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
        ("c", serde_json::json!({"x": 3})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args(["all-docs", db_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], "a");
    assert_eq!(rows[1]["id"], "b");
    assert_eq!(rows[2]["id"], "c");
}

#[tokio::test]
async fn all_docs_include_docs() {
    let (_dir, db_path) = setup_db(&[("doc1", serde_json::json!({"name": "Alice"}))]).await;

    let output = rouchdb_cmd()
        .args(["all-docs", db_path.to_str().unwrap(), "--include-docs"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows[0]["doc"]["_id"], "doc1");
    assert_eq!(rows[0]["doc"]["name"], "Alice");
}

#[tokio::test]
async fn all_docs_limit_and_skip() {
    let (_dir, db_path) = setup_db(&[
        ("a", serde_json::json!({})),
        ("b", serde_json::json!({})),
        ("c", serde_json::json!({})),
        ("d", serde_json::json!({})),
        ("e", serde_json::json!({})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args([
            "all-docs",
            db_path.to_str().unwrap(),
            "--skip",
            "1",
            "--limit",
            "2",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "b");
    assert_eq!(rows[1]["id"], "c");
}

#[tokio::test]
async fn all_docs_descending() {
    let (_dir, db_path) = setup_db(&[
        ("a", serde_json::json!({})),
        ("b", serde_json::json!({})),
        ("c", serde_json::json!({})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args(["all-docs", db_path.to_str().unwrap(), "--descending"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows[0]["id"], "c");
    assert_eq!(rows[1]["id"], "b");
    assert_eq!(rows[2]["id"], "a");
}

#[tokio::test]
async fn all_docs_key_range() {
    let (_dir, db_path) = setup_db(&[
        ("a", serde_json::json!({})),
        ("b", serde_json::json!({})),
        ("c", serde_json::json!({})),
        ("d", serde_json::json!({})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args([
            "all-docs",
            db_path.to_str().unwrap(),
            "--start-key",
            "b",
            "--end-key",
            "c",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "b");
    assert_eq!(rows[1]["id"], "c");
}

// ─── FIND ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn find_with_selector() {
    let (_dir, db_path) = setup_db(&[
        (
            "apple",
            serde_json::json!({"type": "fruit", "name": "Apple"}),
        ),
        (
            "carrot",
            serde_json::json!({"type": "vegetable", "name": "Carrot"}),
        ),
        (
            "banana",
            serde_json::json!({"type": "fruit", "name": "Banana"}),
        ),
    ])
    .await;

    let output = rouchdb_cmd()
        .args([
            "find",
            db_path.to_str().unwrap(),
            "--selector",
            r#"{"type": "fruit"}"#,
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let docs = v["docs"].as_array().unwrap();
    // Without a sort, matches come in _id order.
    let found: Vec<(&str, &str)> = docs
        .iter()
        .map(|d| (d["_id"].as_str().unwrap(), d["name"].as_str().unwrap()))
        .collect();
    assert_eq!(found, [("apple", "Apple"), ("banana", "Banana")]);
}

/// Ids of the documents in a `find` output, in order.
fn found_ids(output: &Output) -> Vec<String> {
    assert!(output.status.success(), "{}", stderr_str(output));
    stdout_json(output)["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn find_sort_skip_and_limit_return_exact_ordered_ids() {
    let (_dir, db_path) = setup_db(&[
        ("p1", serde_json::json!({"name": "Carol", "age": 35})),
        ("p2", serde_json::json!({"name": "Alice", "age": 30})),
        ("p3", serde_json::json!({"name": "Bob", "age": 25})),
        ("p4", serde_json::json!({"name": "Dave", "age": 40})),
        ("other", serde_json::json!({"kind": "no age"})),
    ])
    .await;
    let p = path_str(&db_path);
    let find = |extra: &[&str]| {
        let mut args = vec!["find", p, "--selector", r#"{"age": {"$gt": 0}}"#];
        args.extend_from_slice(extra);
        found_ids(&run(&args))
    };

    assert_eq!(find(&[]), ["p1", "p2", "p3", "p4"]);
    assert_eq!(
        find(&["--sort", r#"[{"age": "asc"}]"#]),
        ["p3", "p2", "p1", "p4"]
    );
    assert_eq!(
        find(&["--sort", r#"[{"age": "desc"}]"#]),
        ["p4", "p1", "p2", "p3"]
    );
    assert_eq!(find(&["--sort", r#"["name"]"#]), ["p2", "p3", "p1", "p4"]);
    assert_eq!(find(&["--skip", "1"]), ["p2", "p3", "p4"]);
    assert_eq!(
        find(&[
            "--sort",
            r#"[{"age": "desc"}]"#,
            "--skip",
            "1",
            "--limit",
            "2"
        ]),
        ["p1", "p2"]
    );
    assert_eq!(find(&["--skip", "4"]), Vec::<String>::new());

    let output = run(&["find", p, "--selector", "{}", "--sort", "not json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_str(&output).contains("invalid sort JSON"),
        "{}",
        stderr_str(&output)
    );
}

#[tokio::test]
async fn find_with_fields() {
    let (_dir, db_path) = setup_db(&[(
        "doc1",
        serde_json::json!({"name": "Alice", "age": 30, "city": "NYC"}),
    )])
    .await;

    let output = rouchdb_cmd()
        .args([
            "find",
            db_path.to_str().unwrap(),
            "--selector",
            r#"{"name": "Alice"}"#,
            "--fields",
            "name",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let docs = v["docs"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert!(docs[0].get("name").is_some());
    // Like CouchDB, only the requested fields are returned (no implicit _id).
    assert!(docs[0].get("_id").is_none());
    // age and city should not be present
    assert!(docs[0].get("age").is_none());
    assert!(docs[0].get("city").is_none());
}

#[tokio::test]
async fn find_invalid_selector_fails() {
    let (_dir, db_path) = setup_db(&[]).await;

    rouchdb_cmd()
        .args([
            "find",
            db_path.to_str().unwrap(),
            "--selector",
            "not valid json",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid selector"));
}

// ─── CHANGES ────────────────────────────────────────────────────────────────

/// a, b and c, then a new revision of a: the feed order is b, c, a.
async fn setup_changes_db() -> (TempDir, PathBuf) {
    let (dir, db_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
        ("c", serde_json::json!({"x": 3})),
    ])
    .await;
    {
        let db = rouchdb::Database::open(&db_path, "test").unwrap();
        let a = db.get("a").await.unwrap();
        db.update(
            "a",
            &a.rev.unwrap().to_string(),
            serde_json::json!({"x": 10}),
        )
        .await
        .unwrap();
    }
    (dir, db_path)
}

/// `(id, seq)` of the rows of a `changes` output, in order, and `last_seq`.
fn change_rows(output: &Output) -> (Vec<(String, u64)>, serde_json::Value) {
    assert!(output.status.success(), "{}", stderr_str(output));
    let v = stdout_json(output);
    let rows = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_string(),
                r["seq"].as_u64().unwrap(),
            )
        })
        .collect();
    (rows, v["last_seq"].clone())
}

fn rows(expected: &[(&str, u64)]) -> Vec<(String, u64)> {
    expected
        .iter()
        .map(|(id, seq)| (id.to_string(), *seq))
        .collect()
}

#[tokio::test]
async fn changes_returns_all() {
    let (_dir, db_path) = setup_changes_db().await;
    let p = path_str(&db_path);

    assert_eq!(
        change_rows(&run(&["changes", p])),
        (rows(&[("b", 2), ("c", 3), ("a", 4)]), serde_json::json!(4))
    );
}

#[tokio::test]
async fn changes_with_limit() {
    let (_dir, db_path) = setup_changes_db().await;
    let p = path_str(&db_path);

    assert_eq!(
        change_rows(&run(&["changes", p, "--limit", "2"])),
        (rows(&[("b", 2), ("c", 3)]), serde_json::json!(3))
    );
}

#[tokio::test]
async fn changes_with_since() {
    let (_dir, db_path) = setup_changes_db().await;
    let p = path_str(&db_path);

    assert_eq!(
        change_rows(&run(&["changes", p, "--since", "2"])),
        (rows(&[("c", 3), ("a", 4)]), serde_json::json!(4))
    );
    assert_eq!(
        change_rows(&run(&["changes", p, "--since", "4"])),
        (rows(&[]), serde_json::json!(4))
    );
}

#[tokio::test]
async fn changes_descending_lists_newest_first() {
    let (_dir, db_path) = setup_changes_db().await;
    let p = path_str(&db_path);

    // As in CouchDB, last_seq is the seq of the last row returned.
    assert_eq!(
        change_rows(&run(&["changes", p, "--descending"])),
        (rows(&[("a", 4), ("c", 3), ("b", 2)]), serde_json::json!(2))
    );
    assert_eq!(
        change_rows(&run(&["changes", p, "--descending", "--limit", "2"])),
        (rows(&[("a", 4), ("c", 3)]), serde_json::json!(3))
    );
}

#[tokio::test]
async fn changes_include_docs_adds_bodies() {
    let (_dir, db_path) = setup_changes_db().await;
    let p = path_str(&db_path);
    let rev_b = stdout_json(&run(&["get", p, "b"]))["_rev"].clone();
    let rev_a = stdout_json(&run(&["get", p, "a"]))["_rev"].clone();
    let rev_c = stdout_json(&run(&["get", p, "c"]))["_rev"].clone();
    let rev_c2 = rev_of(&run(&["delete", p, "c", "--rev", rev_c.as_str().unwrap()]));

    let output = run(&["changes", p, "--include-docs"]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({
            "last_seq": 5,
            "results": [
                {"seq": 2, "id": "b", "changes": [{"rev": rev_b}],
                 "doc": {"_id": "b", "_rev": rev_b, "x": 2}},
                {"seq": 4, "id": "a", "changes": [{"rev": rev_a}],
                 "doc": {"_id": "a", "_rev": rev_a, "x": 10}},
                {"seq": 5, "id": "c", "changes": [{"rev": rev_c2}], "deleted": true,
                 "doc": {"_id": "c", "_rev": rev_c2, "_deleted": true}},
            ]
        })
    );

    // Without the flag the rows are the same minus the bodies.
    let output = run(&["changes", p]);
    let results = stdout_json(&output)["results"].clone();
    let ids: Vec<&str> = results
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["b", "a", "c"]);
    for row in results.as_array().unwrap() {
        assert!(row.get("doc").is_none(), "{}", row);
    }
}

// ─── DUMP ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn dump_exports_all() {
    let (_dir, db_path) = setup_db(&[
        ("doc1", serde_json::json!({"name": "Alice"})),
        ("doc2", serde_json::json!({"name": "Bob"})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args(["dump", db_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(stderr_str(&output), "");
    // Each document at its winning revision, in _id order.
    let p = path_str(&db_path);
    let expected: Vec<serde_json::Value> = ["doc1", "doc2"]
        .iter()
        .map(|id| stdout_json(&run(&["get", p, id])))
        .collect();
    assert_eq!(expected[1]["name"], "Bob");
    assert_eq!(stdout_json(&output), serde_json::Value::Array(expected));
}

#[tokio::test]
async fn dump_empty_database() {
    let (_dir, db_path) = setup_db(&[]).await;

    let output = rouchdb_cmd()
        .args(["dump", db_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 0);
}

// ─── REPLICATE ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn replicate_redb_to_redb_copies_docs_revisions_and_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.redb");
    let s = path_str(&src);
    rev_of(&run(&["put", s, "a", r#"{"x":1}"#]));
    let b1 = rev_of(&run(&["put", s, "b", r#"{"x":2}"#]));
    rev_of(&run(&["put", s, "b", r#"{"x":20}"#, "--rev", &b1]));
    let c1 = rev_of(&run(&["put", s, "c", r#"{"x":3}"#]));
    rev_of(&run(&["delete", s, "c", "--rev", &c1]));
    rev_of(&run(&["put", s, "d", r#"{"v":"main"}"#]));
    {
        let db = rouchdb::Database::open(&src, "src").unwrap();
        let branch = rouchdb::Document {
            id: "d".into(),
            rev: Some(rouchdb::Revision::new(1, "f".repeat(32))),
            deleted: false,
            data: serde_json::json!({"v": "branch"}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![branch], rouchdb::BulkDocsOptions::replication())
            .await
            .unwrap();
        assert!(results.iter().all(|r| r.ok), "{:?}", results);
    }
    let tgt = dir.path().join("tgt.redb");
    let t = path_str(&tgt);

    let output = run(&["replicate", s, t]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    // Four changes, five leaves (both sides of the conflict on d).
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({
            "ok": true, "docs_read": 4, "docs_written": 5, "errors": [], "last_seq": 7
        })
    );

    // The same live documents, at the same revisions, with the same bodies.
    let source_docs = all_docs_with_bodies(&src);
    let ids: Vec<&serde_json::Value> = source_docs.iter().map(|d| &d["_id"]).collect();
    assert_eq!(ids, ["a", "b", "d"]);
    assert_eq!(all_docs_with_bodies(&tgt), source_docs);
    // The same conflict...
    let conflicted = stdout_json(&run(&["get", s, "d", "--conflicts"]));
    assert_eq!(conflicted["_conflicts"].as_array().unwrap().len(), 1);
    assert_eq!(
        stdout_json(&run(&["get", t, "d", "--conflicts"])),
        conflicted
    );
    // ...and the same deletion: the feeds match except for the seqs, which
    // depend on the order the target stored the documents in.
    let feed = |path: &str| {
        let mut rows: Vec<serde_json::Value> = stdout_json(&run(&["changes", path]))["results"]
            .as_array()
            .unwrap()
            .iter()
            .cloned()
            .map(|mut row| {
                row.as_object_mut().unwrap().remove("seq");
                row
            })
            .collect();
        rows.sort_by(|x, y| x["id"].as_str().cmp(&y["id"].as_str()));
        rows
    };
    let source_feed = feed(s);
    assert_eq!(source_feed[2]["id"], "c");
    assert_eq!(source_feed[2]["deleted"], true);
    assert_eq!(feed(t), source_feed);

    // A second run resumes from the checkpoint.
    let output = run(&["replicate", s, t]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({
            "ok": true, "docs_read": 0, "docs_written": 0, "errors": [], "last_seq": 7
        })
    );
}

/// Every live document (`_id`, `_rev` and body), via `all-docs --include-docs`.
fn all_docs_with_bodies(path: &Path) -> Vec<serde_json::Value> {
    let output = run(&["all-docs", path_str(path), "--include-docs"]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    stdout_json(&output)["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["doc"].clone())
        .collect()
}

#[tokio::test]
async fn replicate_selector_copies_only_matching_docs() {
    let (_src_dir, src_path) = setup_db(&[
        ("apple", serde_json::json!({"type": "fruit"})),
        ("banana", serde_json::json!({"type": "fruit"})),
        ("carrot", serde_json::json!({"type": "vegetable"})),
        ("rock", serde_json::json!({})),
    ])
    .await;
    let tgt_dir = tempfile::tempdir().unwrap();
    let tgt_path = tgt_dir.path().join("target.redb");
    let (s, t) = (path_str(&src_path), path_str(&tgt_path));
    let source_docs = all_docs_with_bodies(&src_path);
    let doc = |id: &str| source_docs.iter().find(|d| d["_id"] == id).unwrap().clone();

    let output = run(&["replicate", s, t, "--selector", r#"{"type": "fruit"}"#]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({
            "ok": true, "docs_read": 4, "docs_written": 2, "errors": [], "last_seq": 4
        })
    );
    assert_eq!(
        all_docs_with_bodies(&tgt_path),
        [doc("apple"), doc("banana")]
    );

    // Another selector has its own checkpoint, so it scans the feed again.
    let output = run(&["replicate", s, t, "--selector", r#"{"type": "vegetable"}"#]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({
            "ok": true, "docs_read": 4, "docs_written": 1, "errors": [], "last_seq": 4
        })
    );
    assert_eq!(
        all_docs_with_bodies(&tgt_path),
        [doc("apple"), doc("banana"), doc("carrot")]
    );

    let output = run(&["replicate", s, t, "--selector", "{not json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        stderr_str(&output).contains("invalid selector JSON"),
        "{}",
        stderr_str(&output)
    );
}

#[tokio::test]
async fn replicate_source_and_target_names_select_the_checkpoint() {
    // The replication id, and so the checkpoint, is derived from both
    // database names, which default to the file stems ("test", "target").
    let (_src_dir, src_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
    ])
    .await;
    let tgt_dir = tempfile::tempdir().unwrap();
    let tgt_path = tgt_dir.path().join("target.redb");
    let replicate = |names: &[&str]| {
        let mut args = vec!["replicate", path_str(&src_path), path_str(&tgt_path)];
        args.extend_from_slice(names);
        let output = run(&args);
        assert!(output.status.success(), "{}", stderr_str(&output));
        let v = stdout_json(&output);
        (v["docs_read"].clone(), v["docs_written"].clone())
    };

    assert_eq!(replicate(&[]), (2.into(), 2.into()));
    assert_eq!(
        replicate(&[]),
        (0.into(), 0.into()),
        "resumes from the checkpoint"
    );
    assert_eq!(
        replicate(&["--source-name", "other"]),
        (2.into(), 0.into()),
        "another source name is another replication"
    );
    assert_eq!(replicate(&["--source-name", "other"]), (0.into(), 0.into()));
    assert_eq!(
        replicate(&["--target-name", "other"]),
        (2.into(), 0.into()),
        "another target name is another replication"
    );
    assert_eq!(
        replicate(&["--source-name", "test", "--target-name", "target"]),
        (0.into(), 0.into()),
        "the default names are the file stems"
    );
}

#[ignore = "requires CouchDB"]
#[tokio::test]
async fn replicate_to_couchdb() {
    // The replication creates the target database; the guard deletes it.
    let target = common::unique_remote_db("cli_replicate");

    let (_src_dir, src_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args(["replicate", src_path.to_str().unwrap(), target.url()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["docs_written"], 2);
    let (status, body) = couch_request("GET", target.url(), None);
    assert_eq!(status, 200, "{}", body);
    let info: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(info["doc_count"], 2);
}

// ─── COMPACT ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn compact_returns_ok() {
    let (_dir, db_path) = setup_db(&[("doc1", serde_json::json!({"name": "Alice"}))]).await;

    let output = rouchdb_cmd()
        .args(["compact", db_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["ok"], true);
}

// ─── MISSING DATABASE FILES ─────────────────────────────────────────────────

#[test]
fn read_commands_on_missing_file_fail_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("prod.rdb");
    let p = path_str(&missing);

    let cases: Vec<Vec<&str>> = vec![
        vec!["info", p],
        vec!["get", p, "doc1"],
        vec!["all-docs", p],
        vec!["find", p, "--selector", "{}"],
        vec!["changes", p],
        vec!["dump", p],
        vec!["compact", p],
        vec!["delete", p, "doc1", "--rev", "1-abc"],
    ];
    for args in cases {
        let output = run(&args);
        assert!(
            !output.status.success(),
            "{:?} must fail on a missing file, stdout: {}",
            args,
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr_str(&output).contains("does not exist"),
            "{:?} stderr: {}",
            args,
            stderr_str(&output)
        );
        assert!(!missing.exists(), "{:?} must not create the file", args);
    }
}

#[tokio::test]
async fn replicate_from_missing_redb_source_fails_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.redb");
    let target = dir.path().join("target.redb");

    let output = run(&["replicate", path_str(&missing), path_str(&target)]);
    assert!(!output.status.success());
    assert!(stderr_str(&output).contains("does not exist"));
    assert!(!missing.exists());
}

#[test]
fn write_commands_create_missing_file() {
    let dir = tempfile::tempdir().unwrap();

    let put_path = dir.path().join("put.redb");
    let output = run(&["put", path_str(&put_path), "doc1", r#"{"x":1}"#]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert!(put_path.exists());

    let post_path = dir.path().join("post.redb");
    let output = run(&["post", path_str(&post_path), r#"{"x":1}"#]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert!(post_path.exists());
}

// ─── USAGE ERRORS ───────────────────────────────────────────────────────────

#[test]
fn usage_errors_exit_2_with_empty_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.redb");
    let p = path_str(&path);

    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec![], "Usage: rouchdb [OPTIONS] <COMMAND>"),
        (
            vec!["frobnicate"],
            "error: unrecognized subcommand 'frobnicate'",
        ),
        (
            vec!["info"],
            "error: the following required arguments were not provided:\n  <PATH>",
        ),
        (
            vec!["info", p, "--bogus"],
            "error: unexpected argument '--bogus' found",
        ),
        (vec!["find", p], "\n  --selector <SELECTOR>\n"),
        (vec!["delete", p, "doc1"], "\n  --rev <REV>\n"),
        (vec!["put", p, "doc1"], "\n  <BODY>\n"),
        (vec!["replicate", p], "\n  <TARGET>\n"),
        (
            vec!["all-docs", p, "--limit", "many"],
            "error: invalid value 'many' for '--limit <LIMIT>'",
        ),
    ];
    for (args, message) in cases {
        let output = run(&args);
        let stderr = stderr_str(&output);
        assert_eq!(output.status.code(), Some(2), "{:?}: {}", args, stderr);
        assert!(
            output.stdout.is_empty(),
            "{:?} wrote to stdout: {}",
            args,
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(stderr.contains(message), "{:?}: {}", args, stderr);
        assert!(!path.exists(), "{:?} created the database file", args);
    }
}

// ─── PUT / POST / DELETE ────────────────────────────────────────────────────

#[tokio::test]
async fn put_creates_document() {
    let (_dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);

    let output = run(&["put", p, "doc1", r#"{"name":"Alice"}"#]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["ok"], true);
    assert_eq!(v["id"], "doc1");
    assert!(v["rev"].as_str().unwrap().starts_with("1-"));

    let doc = stdout_json(&run(&["get", p, "doc1"]));
    assert_eq!(doc["name"], "Alice");
}

#[tokio::test]
async fn put_updates_document_with_rev() {
    let (_dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);

    let created = stdout_json(&run(&["put", p, "doc1", r#"{"v":1}"#]));
    let rev1 = created["rev"].as_str().unwrap();

    let output = run(&["put", p, "doc1", r#"{"v":2}"#, "--rev", rev1]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert!(v["rev"].as_str().unwrap().starts_with("2-"));

    let doc = stdout_json(&run(&["get", p, "doc1"]));
    assert_eq!(doc["v"], 2);
}

#[tokio::test]
async fn put_existing_without_rev_is_a_conflict() {
    let (_dir, db_path) = setup_db(&[("doc1", serde_json::json!({"v": 1}))]).await;
    let p = path_str(&db_path);

    let output = run(&["put", p, "doc1", r#"{"v":2}"#]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_str(&output).contains("conflict"));

    let doc = stdout_json(&run(&["get", p, "doc1"]));
    assert_eq!(doc["v"], 1, "a rejected put must not change the doc");
}

#[tokio::test]
async fn put_with_stale_rev_is_a_conflict() {
    let (_dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);

    let rev1 = stdout_json(&run(&["put", p, "doc1", r#"{"v":1}"#]))["rev"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        run(&["put", p, "doc1", r#"{"v":2}"#, "--rev", &rev1])
            .status
            .success()
    );

    let output = run(&["put", p, "doc1", r#"{"v":3}"#, "--rev", &rev1]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_str(&output).contains("conflict"));
}

#[tokio::test]
async fn put_force_upserts_existing_document() {
    let (_dir, db_path) = setup_db(&[("doc1", serde_json::json!({"v": 1}))]).await;
    let p = path_str(&db_path);

    let output = run(&["put", p, "doc1", r#"{"v":2}"#, "--force"]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert!(
        stdout_json(&output)["rev"]
            .as_str()
            .unwrap()
            .starts_with("2-")
    );
    assert_eq!(stdout_json(&run(&["get", p, "doc1"]))["v"], 2);
}

#[tokio::test]
async fn put_force_creates_missing_document() {
    let (_dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);

    let output = run(&["put", p, "doc1", r#"{"v":1}"#, "-f"]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert!(
        stdout_json(&output)["rev"]
            .as_str()
            .unwrap()
            .starts_with("1-")
    );
}

#[tokio::test]
async fn put_invalid_json_fails() {
    let (_dir, db_path) = setup_db(&[]).await;
    let output = run(&["put", path_str(&db_path), "doc1", "{not json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_str(&output).contains("invalid JSON body"));
}

#[tokio::test]
async fn post_generates_id() {
    let (_dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);

    let output = run(&["post", p, r#"{"name":"Bob"}"#]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["ok"], true);
    let id = v["id"].as_str().unwrap();
    assert!(!id.is_empty());

    assert_eq!(stdout_json(&run(&["get", p, id]))["name"], "Bob");
}

#[tokio::test]
async fn delete_removes_document() {
    let (_dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);

    let rev = stdout_json(&run(&["put", p, "doc1", r#"{"v":1}"#]))["rev"]
        .as_str()
        .unwrap()
        .to_string();
    let output = run(&["delete", p, "doc1", "--rev", &rev]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["ok"], true);
    assert!(v["rev"].as_str().unwrap().starts_with("2-"));

    let get = run(&["get", p, "doc1"]);
    assert_eq!(get.status.code(), Some(1));
    assert!(stderr_str(&get).contains("not found"));
}

#[tokio::test]
async fn delete_with_wrong_rev_fails() {
    let (_dir, db_path) = setup_db(&[("doc1", serde_json::json!({"v": 1}))]).await;
    let p = path_str(&db_path);

    let output = run(&[
        "delete",
        p,
        "doc1",
        "--rev",
        "1-00000000000000000000000000000000",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_str(&output).contains("conflict"));
    assert!(run(&["get", p, "doc1"]).status.success());

    let output = run(&["delete", p, "doc1", "--rev", "garbage"]);
    assert_eq!(output.status.code(), Some(1));
}

#[tokio::test]
async fn put_after_delete_recreates_document() {
    let (_dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);

    let rev = stdout_json(&run(&["put", p, "doc1", r#"{"v":1}"#]))["rev"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(run(&["delete", p, "doc1", "--rev", &rev]).status.success());

    let output = run(&["put", p, "doc1", r#"{"v":2}"#]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(stdout_json(&output)["ok"], true);
    assert_eq!(stdout_json(&run(&["get", p, "doc1"]))["v"], 2);

    // --force on a deleted doc also recreates it.
    let rev = stdout_json(&run(&["get", p, "doc1"]))["_rev"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(run(&["delete", p, "doc1", "--rev", &rev]).status.success());
    let output = run(&["put", p, "doc1", r#"{"v":3}"#, "--force"]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(stdout_json(&run(&["get", p, "doc1"]))["v"], 3);
}

// ─── IMPORT ─────────────────────────────────────────────────────────────────

fn write_json_file(dir: &Path, name: &str, value: &serde_json::Value) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    path
}

#[tokio::test]
async fn import_writes_documents() {
    let (dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);
    let file = write_json_file(
        dir.path(),
        "docs.json",
        &serde_json::json!([
            {"_id": "a", "x": 1},
            {"_id": "b", "_rev": "7-deadbeef", "x": 2}
        ]),
    );

    let output = run(&["import", p, path_str(&file)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["ok"], true);
    assert_eq!(v["imported"], 2);
    assert_eq!(v["total"], 2);
    assert_eq!(v["errors"], serde_json::json!([]));

    let b = stdout_json(&run(&["get", p, "b"]));
    assert_eq!(b["x"], 2);
    assert!(b["_rev"].as_str().unwrap().starts_with("1-"));
}

#[tokio::test]
async fn import_reports_docs_without_id() {
    let (dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);
    let file = write_json_file(
        dir.path(),
        "docs.json",
        &serde_json::json!([{"_id": "a", "x": 1}, {"x": 2}, {"_id": "", "x": 3}]),
    );

    let output = run(&["import", p, path_str(&file)]);
    assert_eq!(output.status.code(), Some(1));
    let v = stdout_json(&output);
    assert_eq!(v["ok"], false);
    assert_eq!(v["imported"], 1);
    assert_eq!(v["total"], 3);
    let errors = v["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2, "{:?}", errors);
    assert_eq!(errors[0]["error"], "missing _id field");
    assert_eq!(errors[0]["doc"]["x"], 2);
    assert!(stderr_str(&output).contains("2 of 3 documents failed"));

    assert_eq!(stdout_json(&run(&["info", p]))["doc_count"], 1);
}

#[tokio::test]
async fn import_reports_conflicts_per_id() {
    let (dir, db_path) = setup_db(&[("existing", serde_json::json!({"v": 1}))]).await;
    let p = path_str(&db_path);
    let file = write_json_file(
        dir.path(),
        "docs.json",
        &serde_json::json!([
            {"_id": "existing", "v": 2},
            {"_id": "new", "v": 1},
            {"_id": "dup", "v": 1},
            {"_id": "dup", "v": 2}
        ]),
    );

    let output = run(&["import", p, path_str(&file)]);
    assert_eq!(output.status.code(), Some(1));
    let v = stdout_json(&output);
    assert_eq!(v["imported"], 2);
    assert_eq!(v["total"], 4);
    let errors = v["errors"].as_array().unwrap();
    let ids: Vec<&str> = errors.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["existing", "dup"]);
    for e in errors {
        assert!(e["error"].as_str().unwrap().contains("conflict"), "{:?}", e);
    }

    assert_eq!(stdout_json(&run(&["get", p, "existing"]))["v"], 1);
    assert_eq!(stdout_json(&run(&["get", p, "dup"]))["v"], 1);
}

#[tokio::test]
async fn import_recreates_deleted_document() {
    let (dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);
    let rev = stdout_json(&run(&["put", p, "gone", r#"{"v":1}"#]))["rev"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(run(&["delete", p, "gone", "--rev", &rev]).status.success());

    let file = write_json_file(
        dir.path(),
        "docs.json",
        &serde_json::json!([{"_id": "gone", "v": 2}]),
    );
    let output = run(&["import", p, path_str(&file)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(stdout_json(&output)["imported"], 1);
    assert_eq!(stdout_json(&run(&["get", p, "gone"]))["v"], 2);
}

#[tokio::test]
async fn import_many_docs_across_batches() {
    let (dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);
    let docs: Vec<serde_json::Value> = (0..1200)
        .map(|i| serde_json::json!({"_id": format!("doc{:05}", i), "i": i}))
        .collect();
    let file = write_json_file(dir.path(), "docs.json", &serde_json::json!(docs));

    let output = run(&["import", p, path_str(&file)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["ok"], true);
    assert_eq!(v["imported"], 1200);
    assert_eq!(v["total"], 1200);

    assert_eq!(stdout_json(&run(&["info", p]))["doc_count"], 1200);
    assert_eq!(stdout_json(&run(&["get", p, "doc01199"]))["i"], 1199);
}

#[tokio::test]
async fn import_invalid_file_fails() {
    let (dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);

    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();
    let output = run(&["import", p, path_str(&bad)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_str(&output).contains("invalid JSON"));

    let missing = dir.path().join("missing.json");
    let output = run(&["import", p, path_str(&missing)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_str(&output).contains("cannot read file"));
}

/// `{<members>"v": [[...[1]...]]}`, `depth` containers deep, as text (a
/// value that deep cannot be parsed back with serde_json alone).
fn deep_doc_text(depth: usize, members: &str) -> String {
    format!(
        r#"{{{members}"v":{}1{}}}"#,
        "[".repeat(depth - 1),
        "]".repeat(depth - 1)
    )
}

/// Documents as deeply nested as the database stores them can be written
/// from the command line or a file and found; deeper ones get the
/// database's error.
#[tokio::test]
async fn deep_documents_up_to_the_limit_are_accepted() {
    let (dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);
    let max = rouchdb::MAX_NESTING_DEPTH;

    let output = run(&["put", p, "put", &deep_doc_text(max, "")]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let output = run(&["post", p, &deep_doc_text(max, r#""_id":"posted","#)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let file = dir.path().join("deep.json");
    let docs = format!("[{}]", deep_doc_text(max, r#""_id":"imported","#));
    std::fs::write(&file, docs).unwrap();
    let output = run(&["import", p, path_str(&file)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(stdout_json(&output)["imported"], 1);

    // A selector as deep as the documents finds them.
    let selector = deep_doc_text(max, "");
    let output = run(&["find", p, "--selector", &selector, "--fields", "_id"]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let mut ids: Vec<String> = stdout_json(&output)["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, ["imported", "posted", "put"]);

    let too_deep = format!("Document nesting exceeds the maximum depth of {max}");
    for depth in [max + 1, 5 * max] {
        let output = run(&["put", p, "deeper", &deep_doc_text(depth, "")]);
        assert_eq!(output.status.code(), Some(1), "{depth}");
        assert!(stderr_str(&output).contains(&too_deep), "{depth}");
        let output = run(&["post", p, &deep_doc_text(depth, "")]);
        assert_eq!(output.status.code(), Some(1), "{depth}");
        assert!(stderr_str(&output).contains(&too_deep), "{depth}");
        let docs = format!("[{}]", deep_doc_text(depth, r#""_id":"deeper","#));
        std::fs::write(&file, docs).unwrap();
        let output = run(&["import", p, path_str(&file)]);
        assert_eq!(output.status.code(), Some(1), "{depth}");
        assert!(stderr_str(&output).contains(&too_deep), "{depth}");
    }

    let db = rouchdb::Database::open(&db_path, "test").unwrap();
    assert_eq!(db.info().await.unwrap().doc_count, 3);
    let mut expected = serde_json::json!(1);
    for _ in 1..max {
        expected = serde_json::Value::Array(vec![expected]);
    }
    for id in ["put", "posted", "imported"] {
        assert_eq!(db.get(id).await.unwrap().data["v"], expected, "{id}");
    }
}

// ─── DUMP / IMPORT ROUND TRIP ───────────────────────────────────────────────

async fn setup_db_with_attachment() -> (TempDir, PathBuf) {
    let (dir, db_path) =
        setup_db(&[("plain", serde_json::json!({"name": "no attachments"}))]).await;
    {
        let db = rouchdb::Database::open(&db_path, "test").unwrap();
        let r = db
            .put("doc1", serde_json::json!({"name": "Alice"}))
            .await
            .unwrap();
        let r = db
            .put_attachment(
                "doc1",
                "hello.txt",
                &r.rev.unwrap(),
                b"hello world".to_vec(),
                "text/plain",
            )
            .await
            .unwrap();
        db.put_attachment(
            "doc1",
            "blob.bin",
            &r.rev.unwrap(),
            vec![0, 159, 255, 1, 2],
            "application/octet-stream",
        )
        .await
        .unwrap();
    }
    (dir, db_path)
}

#[tokio::test]
async fn dump_includes_attachments_inline() {
    let (_dir, db_path) = setup_db_with_attachment().await;

    let output = run(&["dump", path_str(&db_path)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let docs = stdout_json(&output);
    let doc1 = docs
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["_id"] == "doc1")
        .unwrap();
    let atts = &doc1["_attachments"];
    assert_eq!(atts["hello.txt"]["content_type"], "text/plain");
    assert_eq!(atts["hello.txt"]["data"], b64(b"hello world"));
    assert_eq!(atts["blob.bin"]["data"], b64(&[0, 159, 255, 1, 2]));

    let plain = docs
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["_id"] == "plain")
        .unwrap();
    assert!(plain.get("_attachments").is_none());
}

#[tokio::test]
async fn dump_then_import_round_trip_preserves_data_and_attachments() {
    let (dir, db_path) = setup_db_with_attachment().await;

    let dump = run(&["dump", path_str(&db_path)]);
    assert!(dump.status.success(), "{}", stderr_str(&dump));
    let dump_file = dir.path().join("backup.json");
    std::fs::write(&dump_file, &dump.stdout).unwrap();

    let restored = dir.path().join("restored.redb");
    let output = run(&["import", path_str(&restored), path_str(&dump_file)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["imported"], 2);

    let doc1 = stdout_json(&run(&["get", path_str(&restored), "doc1"]));
    assert_eq!(doc1["name"], "Alice");
    assert_eq!(
        doc1["_attachments"]["hello.txt"]["content_type"],
        "text/plain"
    );
    assert_eq!(doc1["_attachments"]["hello.txt"]["length"], 11);

    let plain = stdout_json(&run(&["get", path_str(&restored), "plain"]));
    assert_eq!(plain["name"], "no attachments");
    assert!(plain.get("_attachments").is_none());

    {
        let db = rouchdb::Database::open(&restored, "restored").unwrap();
        assert_eq!(
            db.get_attachment("doc1", "hello.txt").await.unwrap(),
            b"hello world"
        );
        assert_eq!(
            db.get_attachment("doc1", "blob.bin").await.unwrap(),
            vec![0, 159, 255, 1, 2]
        );
    }

    // Dumping the restored database yields the same bodies and attachments.
    let redump = stdout_json(&run(&["dump", path_str(&restored)]));
    let strip_revs = |v: serde_json::Value| -> Vec<serde_json::Value> {
        v.as_array()
            .unwrap()
            .iter()
            .cloned()
            .map(|mut d| {
                d.as_object_mut().unwrap().remove("_rev");
                d
            })
            .collect()
    };
    assert_eq!(strip_revs(redump), strip_revs(stdout_json(&dump)));
}

/// The documents of a `dump` output by id, without `_rev` (import gives
/// documents new revisions). Fails on a document dumped twice.
fn dumped_docs(output: &Output) -> BTreeMap<String, serde_json::Value> {
    assert!(output.status.success(), "{}", stderr_str(output));
    let mut docs = BTreeMap::new();
    for mut doc in stdout_json(output).as_array().unwrap().iter().cloned() {
        let obj = doc.as_object_mut().unwrap();
        assert!(obj.remove("_rev").is_some(), "no _rev in {:?}", obj);
        let id = obj["_id"].as_str().unwrap().to_string();
        assert!(
            docs.insert(id.clone(), doc).is_none(),
            "{} dumped twice",
            id
        );
    }
    docs
}

#[tokio::test]
async fn dump_then_import_round_trip_keeps_every_doc_design_doc_and_unicode_id() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.redb");
    // Every document as a dump must show it, without its `_rev`: more than
    // 1000 of them, a design doc and non-ASCII ids.
    let mut expected: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    {
        let db = rouchdb::Database::open(&src, "src").unwrap();
        let mut bodies: Vec<(String, serde_json::Value)> = (0..1234)
            .map(|i| {
                let body = serde_json::json!({"i": i, "even": i % 2 == 0});
                (format!("doc{:05}", i), body)
            })
            .collect();
        bodies.extend([
            (
                "_design/app".to_string(),
                serde_json::json!({
                    "language": "javascript",
                    "views": {"by_i": {"map": "function(doc) { emit(doc.i, null); }"}}
                }),
            ),
            (
                "ñandú".to_string(),
                serde_json::json!({"name": "ñandú", "kind": "ave"}),
            ),
            ("日本".to_string(), serde_json::json!({"país": "日本"})),
            (
                "🦀 crab".to_string(),
                serde_json::json!({"emoji": "🦀", "nested": {"list": [1, "dos", null, 3.5]}}),
            ),
        ]);
        let docs = bodies
            .iter()
            .map(|(id, body)| rouchdb::Document {
                id: id.clone(),
                rev: None,
                deleted: false,
                data: body.clone(),
                attachments: HashMap::new(),
            })
            .collect();
        let results = db
            .bulk_docs(docs, rouchdb::BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(results.iter().all(|r| r.ok), "{:?}", results);
        for (id, body) in bodies {
            let mut doc = body;
            doc["_id"] = id.clone().into();
            expected.insert(id, doc);
        }

        for (id, name, content_type, data) in [
            ("ñandú", "foto ñ.png", "image/png", vec![0u8, 255, 1, 128]),
            ("doc00007", "notes.txt", "text/plain", b"hola".to_vec()),
        ] {
            let rev = db.get(id).await.unwrap().rev.unwrap().to_string();
            db.put_attachment(id, name, &rev, data.clone(), content_type)
                .await
                .unwrap();
            expected.get_mut(id).unwrap()["_attachments"] = serde_json::json!({
                name: {"content_type": content_type, "data": b64(&data)}
            });
        }
    }
    assert_eq!(expected.len(), 1238);

    let dump = run(&["dump", path_str(&src)]);
    assert_eq!(stderr_str(&dump), "");
    assert_eq!(dumped_docs(&dump), expected);

    let backup = dir.path().join("backup.json");
    std::fs::write(&backup, &dump.stdout).unwrap();
    let restored = dir.path().join("restored.redb");
    let output = run(&["import", path_str(&restored), path_str(&backup)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({"ok": true, "imported": 1238, "total": 1238, "errors": []})
    );

    assert_eq!(dumped_docs(&run(&["dump", path_str(&restored)])), expected);
}

#[tokio::test]
async fn import_rejects_attachment_stubs_without_data() {
    let (dir, db_path) = setup_db(&[]).await;
    let p = path_str(&db_path);
    let file = write_json_file(
        dir.path(),
        "docs.json",
        &serde_json::json!([
            {"_id": "ok", "x": 1},
            {"_id": "stub", "_attachments": {"a.txt": {
                "content_type": "text/plain", "digest": "md5-x", "length": 3, "stub": true
            }}},
            {"_id": "badb64", "_attachments": {"a.txt": {
                "content_type": "text/plain", "data": "!!!not base64!!!"
            }}}
        ]),
    );

    let output = run(&["import", p, path_str(&file)]);
    assert_eq!(output.status.code(), Some(1));
    let v = stdout_json(&output);
    assert_eq!(v["imported"], 1);
    let ids: Vec<&str> = v["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["stub", "badb64"]);
    assert!(!run(&["get", p, "stub"]).status.success());
}

#[tokio::test]
async fn dump_warns_about_conflicting_revisions() {
    let (_dir, db_path) = setup_db(&[
        ("clean", serde_json::json!({"v": 1})),
        ("conflicted", serde_json::json!({"v": 1})),
    ])
    .await;
    {
        let db = rouchdb::Database::open(&db_path, "test").unwrap();
        let branch = rouchdb::Document {
            id: "conflicted".into(),
            rev: Some(rouchdb::Revision::new(1, "f".repeat(32))),
            deleted: false,
            data: serde_json::json!({"v": 99}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![branch], rouchdb::BulkDocsOptions::replication())
            .await
            .unwrap();
        assert!(results.iter().all(|r| r.ok));
    }

    let output = run(&["dump", path_str(&db_path)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let stderr = stderr_str(&output);
    assert!(stderr.contains("warning"), "stderr: {}", stderr);
    assert!(stderr.contains("conflicted"), "stderr: {}", stderr);
    assert!(!stderr.contains("clean"), "stderr: {}", stderr);

    let docs = stdout_json(&output);
    assert_eq!(docs.as_array().unwrap().len(), 2);
    for d in docs.as_array().unwrap() {
        assert!(d.get("_conflicts").is_none(), "{:?}", d);
    }
}

// ─── REPLICATE RESULTS ──────────────────────────────────────────────────────

#[tokio::test]
async fn replicate_with_rejected_docs_exits_non_zero() {
    let (_src_dir, src_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
    ])
    .await;
    let fake = spawn_fake_couchdb(
        FakeConfig {
            reject_writes: true,
            ..Default::default()
        },
        vec![],
    );

    let output = run(&["replicate", path_str(&src_path), &fake.db_url()]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let v = stdout_json(&output);
    assert_eq!(v["ok"], false);
    assert_eq!(v["docs_read"], 2);
    assert_eq!(v["docs_written"], 0);
    // The replicator writes a batch in no particular order.
    let mut errors: Vec<&str> = v["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    errors.sort_unstable();
    assert_eq!(
        errors,
        [
            "write error for a: forbidden: rejected by validator",
            "write error for b: forbidden: rejected by validator",
        ]
    );
    // Forbidden docs are reported but, as in PouchDB, do not block progress:
    // the checkpoint moves past them so the replication cannot wedge.
    assert_eq!(
        v["last_seq"], 2,
        "checkpoint moves past docs the target forbids"
    );
    let checkpoints: Vec<serde_json::Value> = fake.state().local.values().cloned().collect();
    assert_eq!(checkpoints.len(), 1, "{:?}", checkpoints);
    assert_eq!(checkpoints[0]["last_seq"], 2);
    assert_eq!(
        stderr_str(&output),
        "Error: database error: replication incomplete: 2 error(s), see \"errors\" in the output\n"
    );
    fake.assert_no_problems();
    assert_eq!(fake.stored_revs(), Vec::<serde_json::Value>::new());
}

/// The `_revisions` of revision `revs[0]`, whose ancestors are `revs[1..]`
/// (newest first).
fn revisions(revs: &[&str]) -> serde_json::Value {
    let (start, _) = parse_rev(revs[0]).unwrap();
    let ids: Vec<&str> = revs.iter().map(|r| parse_rev(r).unwrap().1).collect();
    serde_json::json!({"start": start, "ids": ids})
}

fn rev_of(result: &Output) -> String {
    assert!(result.status.success(), "{}", stderr_str(result));
    stdout_json(result)["rev"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn replicate_to_couchdb_sends_revision_history_and_resumes_from_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.redb");
    let p = path_str(&src);
    let a1 = rev_of(&run(&["put", p, "a", r#"{"x":1}"#]));
    let a2 = rev_of(&run(&["put", p, "a", r#"{"x":2}"#, "--rev", &a1]));
    let b1 = rev_of(&run(&["put", p, "b", r#"{"y":1}"#]));
    let c1 = rev_of(&run(&["put", p, "c", r#"{"z":1}"#]));
    let c2 = rev_of(&run(&["delete", p, "c", "--rev", &c1]));
    let fake = spawn_fake_couchdb(FakeConfig::default(), vec![]);

    let output = run(&["replicate", p, &fake.db_url()]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({
            "ok": true, "docs_read": 3, "docs_written": 3, "errors": [], "last_seq": 5
        })
    );
    // Each leaf arrives with its whole history, so CouchDB can graft it
    // onto the revisions it already has.
    assert_eq!(
        fake.stored_revs(),
        vec![
            serde_json::json!({"_id": "a", "_rev": a2, "_revisions": revisions(&[&a2, &a1]), "x": 2}),
            serde_json::json!({"_id": "b", "_rev": b1, "_revisions": revisions(&[&b1]), "y": 1}),
            serde_json::json!({
                "_id": "c", "_rev": c2, "_deleted": true, "_revisions": revisions(&[&c2, &c1])
            }),
        ]
    );
    let checkpoint = {
        let state = fake.state();
        assert_eq!(state.local.len(), 1, "{:?}", state.local);
        let (id, doc) = state.local.iter().next().unwrap();
        assert_eq!(doc["_rev"], "0-1");
        assert_eq!(doc["last_seq"], 5);
        id.clone()
    };

    // Nothing changed: the checkpoint says so, nothing is diffed or written.
    let seen = fake.requests().len();
    let output = run(&["replicate", p, &fake.db_url()]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({
            "ok": true, "docs_read": 0, "docs_written": 0, "errors": [], "last_seq": 5
        })
    );
    let routes: Vec<String> = fake.requests()[seen..]
        .iter()
        .map(|r| format!("{} {}", r.method, r.route()))
        .collect();
    assert!(routes.iter().all(|r| r.starts_with("GET ")), "{:?}", routes);

    // A new revision of b: only it is sent, on top of the revision the
    // target has, and the checkpoint is updated in place (0-1 -> 0-2).
    let b2 = rev_of(&run(&["put", p, "b", r#"{"y":2}"#, "--rev", &b1]));
    let seen = fake.requests().len();
    let output = run(&["replicate", p, &fake.db_url()]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({
            "ok": true, "docs_read": 1, "docs_written": 1, "errors": [], "last_seq": 6
        })
    );
    let requests = fake.requests()[seen..].to_vec();
    let revs_diff = requests
        .iter()
        .find(|r| r.route() == "/db/_revs_diff")
        .expect("a _revs_diff request");
    assert_eq!(revs_diff.json().unwrap(), serde_json::json!({"b": [b2]}));
    let checkpoint_puts = requests
        .iter()
        .filter(|r| r.method == "PUT" && r.route() == format!("/db/_local/{}", checkpoint))
        .count();
    assert_eq!(
        checkpoint_puts, 1,
        "one checkpoint write, without a 409 retry"
    );
    assert_eq!(
        fake.stored_revs()[1..3],
        [
            serde_json::json!({"_id": "b", "_rev": b1, "_revisions": revisions(&[&b1]), "y": 1}),
            serde_json::json!({"_id": "b", "_rev": b2, "_revisions": revisions(&[&b2, &b1]), "y": 2}),
        ]
    );
    let state = fake.state();
    assert_eq!(state.local.len(), 1);
    assert_eq!(state.local[&checkpoint]["_rev"], "0-2");
    assert_eq!(state.local[&checkpoint]["last_seq"], 6);
    drop(state);
    fake.assert_no_problems();
}

#[tokio::test]
async fn replicate_from_couchdb_copies_every_leaf_with_its_history() {
    let h = |c: char| c.to_string().repeat(32);
    let plain = format!("1-{}", h('a'));
    let (edited1, edited2) = (format!("1-{}", h('b')), format!("2-{}", h('c')));
    let (loser, winner) = (format!("1-{}", h('d')), format!("1-{}", h('e')));
    let (gone1, gone2) = (format!("1-{}", h('f')), format!("2-{}", h('1')));
    let fake = spawn_fake_couchdb(
        FakeConfig::default(),
        vec![
            fake_rev("plain", &plain, &[], serde_json::json!({"v": "plain"})),
            fake_rev("edited", &edited1, &[], serde_json::json!({"v": 1})),
            fake_rev("edited", &edited2, &[&h('b')], serde_json::json!({"v": 2})),
            fake_rev("conflicted", &loser, &[], serde_json::json!({"v": "loser"})),
            fake_rev(
                "conflicted",
                &winner,
                &[],
                serde_json::json!({"v": "winner"}),
            ),
            fake_rev("gone", &gone1, &[], serde_json::json!({"v": 0})),
            fake_rev(
                "gone",
                &gone2,
                &[&h('f')],
                serde_json::json!({"_deleted": true}),
            ),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("local.redb");
    let t = path_str(&target);

    let output = run(&["replicate", &fake.db_url(), t]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    // Four changes; five leaves (both sides of the conflict).
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({
            "ok": true, "docs_read": 4, "docs_written": 5, "errors": [], "last_seq": 7
        })
    );
    fake.assert_no_problems();

    let ids: Vec<serde_json::Value> = stdout_json(&run(&["all-docs", t]))["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].clone())
        .collect();
    assert_eq!(ids, ["conflicted", "edited", "plain"]);
    assert_eq!(
        stdout_json(&run(&["get", t, "conflicted", "--conflicts"])),
        serde_json::json!({
            "_id": "conflicted", "_rev": winner, "v": "winner", "_conflicts": [loser]
        })
    );
    assert_eq!(
        stdout_json(&run(&["get", t, "edited"])),
        serde_json::json!({"_id": "edited", "_rev": edited2, "v": 2})
    );
    let gone = run(&["get", t, "gone"]);
    assert_eq!(gone.status.code(), Some(1));
    assert!(
        stderr_str(&gone).contains("not found"),
        "{}",
        stderr_str(&gone)
    );
    {
        let db = rouchdb::Database::open(&target, "local").unwrap();
        let edited = db
            .get_with_opts(
                "edited",
                rouchdb::GetOptions {
                    revs: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            edited.to_json()["_revisions"],
            revisions(&[&edited2, &edited1])
        );
    }

    // The checkpoint was saved on the source as well.
    let state = fake.state();
    let checkpoints: Vec<&serde_json::Value> = state.local.values().collect();
    assert_eq!(checkpoints.len(), 1, "{:?}", checkpoints);
    assert_eq!(checkpoints[0]["last_seq"], 7);
}

#[ignore = "requires CouchDB"]
#[tokio::test]
async fn replicate_rejected_by_couchdb_validator_exits_non_zero() {
    let db_url = common::fresh_remote_db("cli_vdu").await;
    let (status, body) = couch_request(
        "PUT",
        &format!("{}/_design/v", db_url),
        Some(
            r#"{"validate_doc_update":"function(d){ if(d.bad){ throw({forbidden: 'bad docs are rejected'}); } }"}"#,
        ),
    );
    assert_eq!(status, 201, "{}", body);

    let (_src_dir, src_path) = setup_db(&[
        ("good", serde_json::json!({"x": 1})),
        ("evil", serde_json::json!({"bad": true})),
    ])
    .await;
    let output = run(&["replicate", path_str(&src_path), db_url.url()]);

    assert_eq!(output.status.code(), Some(1), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["ok"], false);
    assert_eq!(v["docs_written"], 1);
    let errors = v["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].as_str().unwrap().contains("evil"));
}

// ─── REPLICATE CREDENTIALS ──────────────────────────────────────────────────

#[tokio::test]
async fn replicate_error_does_not_print_url_password() {
    let (_src_dir, src_path) = setup_db(&[("a", serde_json::json!({"x": 1}))]).await;
    let port = closed_port();

    for url in [
        format!("http://admin:s3cret@127.0.0.1:{}/db", port),
        // reqwest cannot percent-decode this username, so it leaves the
        // credentials in the URL it reports in its error message.
        format!("http://ad%FFmin:s3cret@127.0.0.1:{}/db", port),
    ] {
        let output = run(&["replicate", path_str(&src_path), &url]);
        assert_eq!(output.status.code(), Some(1));
        let stderr = stderr_str(&output);
        assert!(stderr.contains("Error"), "{}", stderr);
        assert!(!stderr.contains("s3cret"), "password leaked: {}", stderr);

        let output = run(&["replicate", &url, path_str(&src_path)]);
        assert_eq!(output.status.code(), Some(1));
        let stderr = stderr_str(&output);
        assert!(!stderr.contains("s3cret"), "password leaked: {}", stderr);
    }
}

/// Assert that the fake saw requests and that every one of them carried
/// `authorization` (`None`: no Authorization header at all).
fn assert_every_request_authorization(fake: &FakeCouch, authorization: Option<&str>) {
    let requests = fake.requests();
    assert!(!requests.is_empty(), "the fake CouchDB saw no request");
    for req in &requests {
        assert_eq!(
            req.header("authorization"),
            authorization,
            "{} {}",
            req.method,
            req.path
        );
    }
}

#[tokio::test]
async fn replicate_uses_credentials_from_env() {
    let (_src_dir, src_path) = setup_db(&[("a", serde_json::json!({"x": 1}))]).await;
    let fake = spawn_fake_couchdb(FakeConfig::default(), vec![]);
    let password = "p@ss:w/rd %?#";

    let output = rouchdb_cmd()
        .args(["replicate", path_str(&src_path), &fake.db_url()])
        .env("ROUCHDB_USER", "alice")
        .env("ROUCHDB_PASSWORD", password)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(stdout_json(&output)["docs_written"], 1);

    let expected = format!("Basic {}", b64(format!("alice:{}", password).as_bytes()));
    assert_every_request_authorization(&fake, Some(&expected));
    fake.assert_no_problems();
    let stored: Vec<String> = fake
        .stored_revs()
        .iter()
        .map(|d| doc_id(d).to_string())
        .collect();
    assert_eq!(stored, ["a"]);
}

#[tokio::test]
async fn replicate_env_credentials_apply_to_url_source() {
    let rev = format!("1-{}", "a".repeat(32));
    let fake = spawn_fake_couchdb(
        FakeConfig::default(),
        vec![fake_rev("a", &rev, &[], serde_json::json!({"x": 1}))],
    );
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("local.redb");

    let output = rouchdb_cmd()
        .args(["replicate", &fake.db_url(), path_str(&target)])
        .env("ROUCHDB_USER", "alice")
        .env("ROUCHDB_PASSWORD", "s3cret")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["docs_written"], 1);

    let expected = format!("Basic {}", b64(b"alice:s3cret"));
    assert_every_request_authorization(&fake, Some(&expected));
    fake.assert_no_problems();
    assert_eq!(
        stdout_json(&run(&["get", path_str(&target), "a"])),
        serde_json::json!({"_id": "a", "_rev": rev, "x": 1})
    );
}

#[tokio::test]
async fn replicate_ignores_empty_env_user() {
    // An empty ROUCHDB_USER means "no credentials", even with a password set.
    let (_src_dir, src_path) = setup_db(&[("a", serde_json::json!({"x": 1}))]).await;
    let fake = spawn_fake_couchdb(FakeConfig::default(), vec![]);

    let output = rouchdb_cmd()
        .args(["replicate", path_str(&src_path), &fake.db_url()])
        .env("ROUCHDB_USER", "")
        .env("ROUCHDB_PASSWORD", "s3cret")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(stdout_json(&output)["docs_written"], 1);
    assert_every_request_authorization(&fake, None);
    fake.assert_no_problems();
}

#[tokio::test]
async fn replicate_url_credentials_take_precedence_over_env() {
    let (_src_dir, src_path) = setup_db(&[("a", serde_json::json!({"x": 1}))]).await;
    let fake = spawn_fake_couchdb(FakeConfig::default(), vec![]);

    let output = rouchdb_cmd()
        .args([
            "replicate",
            path_str(&src_path),
            &fake.db_url_with_userinfo("bob:hunter2"),
        ])
        .env("ROUCHDB_USER", "alice")
        .env("ROUCHDB_PASSWORD", "other")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr_str(&output));

    let expected = format!("Basic {}", b64(b"bob:hunter2"));
    assert_every_request_authorization(&fake, Some(&expected));
    fake.assert_no_problems();
}

#[tokio::test]
async fn replicate_errors_in_output_do_not_print_url_password() {
    let (_src_dir, src_path) = setup_db(&[("a", serde_json::json!({"x": 1}))]).await;

    // (credentials in the URL, how they appear in the reported error)
    for (userinfo, shown) in [
        // reqwest strips credentials it can decode from its error URLs...
        ("admin:s3cret", ""),
        // ...but keeps them when it cannot percent-decode the username.
        ("ad%FFmin:s3cret", "ad%FFmin:***@"),
    ] {
        // The checkpoint write fails with a network error naming its URL.
        let fake = spawn_fake_couchdb(
            FakeConfig {
                hang_up_on_checkpoint: true,
                ..Default::default()
            },
            vec![],
        );
        let output = run(&[
            "replicate",
            path_str(&src_path),
            &fake.db_url_with_userinfo(userinfo),
        ]);
        assert_eq!(output.status.code(), Some(1), "{}", stderr_str(&output));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = stderr_str(&output);
        assert!(!stdout.contains("s3cret"), "password leaked: {}", stdout);
        assert!(!stderr.contains("s3cret"), "password leaked: {}", stderr);

        let checkpoint_put = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "PUT")
            .expect("a checkpoint write");
        let v = stdout_json(&output);
        assert_eq!(
            v["errors"],
            serde_json::json!([format!(
                "checkpoint write failed: database error: error sending request for url (http://{}{}{})",
                shown,
                fake.base.trim_start_matches("http://"),
                checkpoint_put.path
            )])
        );
        assert_eq!(v["docs_written"], 1);
        assert_eq!(
            stderr,
            "Error: database error: replication incomplete: 1 error(s), see \"errors\" in the output\n"
        );
        fake.assert_no_problems();
    }
}

#[ignore = "requires CouchDB"]
#[tokio::test]
async fn replicate_to_couchdb_with_env_credentials() {
    let couch = common::couchdb();
    let db = common::fresh_remote_db("cli_env_auth").await;
    // The same URL without credentials; they come from the environment.
    let plain_db_url = db.anonymous_url();

    let (_src_dir, src_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
    ])
    .await;

    let without_env = rouchdb_cmd()
        .args(["replicate", path_str(&src_path), &plain_db_url])
        .env_remove("ROUCHDB_USER")
        .env_remove("ROUCHDB_PASSWORD")
        .output()
        .unwrap();
    let with_env = rouchdb_cmd()
        .args(["replicate", path_str(&src_path), &plain_db_url])
        .env("ROUCHDB_USER", &couch.user)
        .env("ROUCHDB_PASSWORD", &couch.password)
        .output()
        .unwrap();

    assert_eq!(without_env.status.code(), Some(1));
    assert!(stderr_str(&without_env).contains("unauthorized"));
    assert!(with_env.status.success(), "{}", stderr_str(&with_env));
    let v = stdout_json(&with_env);
    assert_eq!(v["ok"], true);
    assert_eq!(v["docs_written"], 2);
}

#[ignore = "requires CouchDB"]
#[tokio::test]
async fn replicate_from_couchdb_with_env_credentials() {
    let couch = common::couchdb();
    let db = common::fresh_remote_db("cli_src_auth").await;
    // The same URL without credentials; they come from the environment.
    let plain_db_url = db.anonymous_url();

    let (status, body) = couch_request("PUT", &format!("{}/a", db.url()), Some(r#"{"x":1}"#));
    assert_eq!(status, 201, "{}", body);
    let rev = serde_json::from_str::<serde_json::Value>(&body).unwrap()["rev"].clone();
    let dir = tempfile::tempdir().unwrap();
    let without_path = dir.path().join("without.redb");
    let with_path = dir.path().join("with.redb");
    let without_env = rouchdb_cmd()
        .args(["replicate", &plain_db_url, path_str(&without_path)])
        .env_remove("ROUCHDB_USER")
        .env_remove("ROUCHDB_PASSWORD")
        .output()
        .unwrap();
    let with_env = rouchdb_cmd()
        .args(["replicate", &plain_db_url, path_str(&with_path)])
        .env("ROUCHDB_USER", &couch.user)
        .env("ROUCHDB_PASSWORD", &couch.password)
        .output()
        .unwrap();

    assert_eq!(without_env.status.code(), Some(1));
    assert_eq!(stderr_str(&without_env), "Error: unauthorized\n");
    assert!(with_env.status.success(), "{}", stderr_str(&with_env));
    let v = stdout_json(&with_env);
    assert_eq!(v["ok"], true);
    assert_eq!(v["docs_written"], 1);
    assert_eq!(
        stdout_json(&run(&["get", path_str(&with_path), "a"])),
        serde_json::json!({"_id": "a", "_rev": rev, "x": 1})
    );
}

// ─── BROKEN PIPE ────────────────────────────────────────────────────────────

#[tokio::test]
async fn dump_to_closed_pipe_exits_cleanly() {
    // ~2000 docs of ~150 bytes: far more than a pipe buffer holds.
    let (_dir, db_path) = setup_bulk_db(2000, 128).await;

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rouchdb"))
        .args(["dump", path_str(&db_path)])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut head = [0u8; 100];
    stdout.read_exact(&mut head).unwrap();
    drop(stdout);
    assert!(
        head.starts_with(br#"[{"_id":"doc00000","#),
        "{}",
        String::from_utf8_lossy(&head)
    );

    // A reader that goes away is not an error: nothing is reported.
    let output = child.wait_with_output().unwrap();
    let stderr = stderr_str(&output);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr);
    assert_eq!(stderr, "");
}

#[cfg(unix)]
#[tokio::test]
async fn output_write_errors_other_than_broken_pipe_fail() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    // ~4000 docs of ~280 bytes: far more than a socket buffer holds.
    let (_dir, db_path) = setup_bulk_db(4000, 256).await;
    // A non-blocking stdout whose reader is still there but never reads:
    // once the buffer is full, writing fails with WouldBlock. Unlike a
    // closed pipe, that is an error the user has to hear about, since the
    // output is incomplete.
    let (reader, writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rouchdb"))
        .args(["all-docs", path_str(&db_path), "--include-docs"])
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .output()
        .unwrap();
    drop(reader);

    let stderr = stderr_str(&output);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr);
    assert!(
        stderr.starts_with("Error writing output: ") && stderr.lines().count() == 1,
        "stderr: {}",
        stderr
    );
}
