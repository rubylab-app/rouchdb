use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use base64::Engine;
use predicates::prelude::*;
use tempfile::TempDir;

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

/// A request captured by the fake CouchDB server.
#[derive(Debug, Clone)]
struct FakeRequest {
    method: String,
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

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
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

/// Serve a minimal CouchDB stand-in on an ephemeral port, one request per
/// connection. Returns the base URL and the log of received requests.
fn spawn_fake_couchdb<F>(handler: F) -> (String, Arc<Mutex<Vec<FakeRequest>>>)
where
    F: Fn(&FakeRequest) -> (u16, serde_json::Value) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let log = Arc::new(Mutex::new(Vec::new()));
    let log_clone = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(req) = read_request(&stream) else {
                continue;
            };
            let (status, body) = handler(&req);
            log_clone.lock().unwrap().push(req);
            let body = body.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                body.len(),
                body
            );
        }
    });
    (base, log)
}

/// Handler emulating an empty CouchDB database named `db`. When
/// `reject_writes` is set, `_bulk_docs` rejects every document the way a
/// `validate_doc_update` function does (per-doc `forbidden`, HTTP 201).
fn fake_target(reject_writes: bool) -> impl Fn(&FakeRequest) -> (u16, serde_json::Value) {
    move |req| {
        let path = req.path.split('?').next().unwrap_or("");
        match (req.method.as_str(), path) {
            ("GET", "/db") => (
                200,
                serde_json::json!({
                    "db_name": "db", "doc_count": 0, "doc_del_count": 0, "update_seq": "0"
                }),
            ),
            ("GET", p) if p.starts_with("/db/_local/") => (
                404,
                serde_json::json!({"error": "not_found", "reason": "missing"}),
            ),
            ("PUT", p) if p.starts_with("/db/_local/") => (
                201,
                serde_json::json!({"ok": true, "id": "_local/x", "rev": "0-1"}),
            ),
            ("POST", "/db/_revs_diff") => {
                let mut out = serde_json::Map::new();
                for (id, revs) in req.json().as_object().unwrap() {
                    out.insert(id.clone(), serde_json::json!({"missing": revs}));
                }
                (200, serde_json::Value::Object(out))
            }
            ("POST", "/db/_bulk_docs") if reject_writes => {
                let results: Vec<serde_json::Value> = req.json()["docs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "id": d["_id"], "rev": d["_rev"],
                            "error": "forbidden", "reason": "rejected by validator"
                        })
                    })
                    .collect();
                (201, serde_json::Value::Array(results))
            }
            // new_edits=false: an empty array means every doc was stored.
            ("POST", "/db/_bulk_docs") => (201, serde_json::json!([])),
            _ => (
                404,
                serde_json::json!({"error": "not_found", "reason": "missing"}),
            ),
        }
    }
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

fn couchdb_url() -> String {
    std::env::var("COUCHDB_URL").unwrap_or_else(|_| "http://admin:password@localhost:15984".into())
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
async fn info_nonexistent_path_fails() {
    // Use a path under a nonexistent directory so redb can't create the file
    rouchdb_cmd()
        .args(["info", "/tmp/no_such_dir_rouchdb/no_such.redb"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Error"));
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
        db.update("doc1", &rev1, serde_json::json!({"version": 2}))
            .await
            .unwrap();
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
    assert_eq!(docs.len(), 2);
    let names: Vec<&str> = docs.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Apple"));
    assert!(names.contains(&"Banana"));
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
    assert!(docs[0].get("_id").is_some());
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

#[tokio::test]
async fn changes_returns_all() {
    let (_dir, db_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
        ("c", serde_json::json!({"x": 3})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args(["changes", db_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert!(v["last_seq"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn changes_with_limit() {
    let (_dir, db_path) = setup_db(&[
        ("a", serde_json::json!({})),
        ("b", serde_json::json!({})),
        ("c", serde_json::json!({})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args(["changes", db_path.to_str().unwrap(), "--limit", "2"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn changes_with_since() {
    let (_dir, db_path) = setup_db(&[
        ("a", serde_json::json!({})),
        ("b", serde_json::json!({})),
        ("c", serde_json::json!({})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args(["changes", db_path.to_str().unwrap(), "--since", "2"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
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

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let docs = v.as_array().unwrap();
    assert_eq!(docs.len(), 2);
    assert!(docs[0].get("_id").is_some());
    assert!(docs[0].get("_rev").is_some());
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
async fn replicate_redb_to_redb() {
    // Set up source with 3 docs
    let (_src_dir, src_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
        ("c", serde_json::json!({"x": 3})),
    ])
    .await;

    // Create empty target
    let (_tgt_dir, tgt_path) = setup_db(&[]).await;

    let output = rouchdb_cmd()
        .args([
            "replicate",
            src_path.to_str().unwrap(),
            tgt_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["docs_written"], 3);

    // Verify target has docs
    let output2 = rouchdb_cmd()
        .args(["info", tgt_path.to_str().unwrap()])
        .output()
        .unwrap();

    let info: serde_json::Value = serde_json::from_slice(&output2.stdout).unwrap();
    assert_eq!(info["doc_count"], 3);
}

#[ignore]
#[tokio::test]
async fn replicate_to_couchdb() {
    let couchdb_url = std::env::var("COUCHDB_URL")
        .unwrap_or_else(|_| "http://admin:password@localhost:15984".to_string());
    let target_url = format!("{}/rouchdb_cli_test_{}", couchdb_url, std::process::id());

    let (_src_dir, src_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
    ])
    .await;

    let output = rouchdb_cmd()
        .args(["replicate", src_path.to_str().unwrap(), &target_url])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["docs_written"], 2);
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

#[tokio::test]
async fn compact_nonexistent_fails() {
    // Use a path under a nonexistent directory so redb can't create the file
    rouchdb_cmd()
        .args(["compact", "/tmp/no_such_dir_rouchdb/no_such.redb"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Error"));
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

// ─── REPLICATE RESULTS ──────────────────────────────────────────────────────

#[tokio::test]
async fn replicate_output_includes_errors_and_last_seq() {
    let (_src_dir, src_path) = setup_db(&[("a", serde_json::json!({"x": 1}))]).await;
    let (_tgt_dir, tgt_path) = setup_db(&[]).await;

    let output = run(&["replicate", path_str(&src_path), path_str(&tgt_path)]);
    assert!(output.status.success(), "{}", stderr_str(&output));
    let v = stdout_json(&output);
    assert_eq!(v["ok"], true);
    assert_eq!(v["errors"], serde_json::json!([]));
    assert_eq!(v["last_seq"], 1);
}

#[tokio::test]
async fn replicate_with_rejected_docs_exits_non_zero() {
    let (_src_dir, src_path) = setup_db(&[
        ("a", serde_json::json!({"x": 1})),
        ("b", serde_json::json!({"x": 2})),
    ])
    .await;
    let (base, _log) = spawn_fake_couchdb(fake_target(true));

    let output = run(&["replicate", path_str(&src_path), &format!("{}/db", base)]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let v = stdout_json(&output);
    assert_eq!(v["ok"], false);
    assert_eq!(v["docs_written"], 0);
    let errors = v["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2, "{:?}", errors);
    assert!(
        errors
            .iter()
            .all(|e| e.as_str().unwrap().contains("rejected by validator"))
    );
    assert_eq!(
        v["last_seq"], 0,
        "checkpoint must not pass the failed batch"
    );
    assert!(stderr_str(&output).contains("replication"));
}

#[ignore]
#[tokio::test]
async fn replicate_rejected_by_couchdb_validator_exits_non_zero() {
    let db_url = format!("{}/rouchdb_cli_vdu_{}", couchdb_url(), std::process::id());
    let (status, body) = couch_request("PUT", &db_url, None);
    assert!(status == 201 || status == 202, "{} {}", status, body);
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
    let output = run(&["replicate", path_str(&src_path), &db_url]);
    couch_request("DELETE", &db_url, None);

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

#[tokio::test]
async fn replicate_uses_credentials_from_env() {
    let (_src_dir, src_path) = setup_db(&[("a", serde_json::json!({"x": 1}))]).await;
    let (base, log) = spawn_fake_couchdb(fake_target(false));
    let password = "p@ss:w/rd %?#";

    let output = rouchdb_cmd()
        .args(["replicate", path_str(&src_path), &format!("{}/db", base)])
        .env("ROUCHDB_USER", "alice")
        .env("ROUCHDB_PASSWORD", password)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr_str(&output));
    assert_eq!(stdout_json(&output)["docs_written"], 1);

    let expected = format!("Basic {}", b64(format!("alice:{}", password).as_bytes()));
    let log = log.lock().unwrap();
    assert!(!log.is_empty());
    for req in log.iter() {
        assert_eq!(
            req.header("authorization"),
            Some(expected.as_str()),
            "{} {}",
            req.method,
            req.path
        );
    }
}

#[tokio::test]
async fn replicate_url_credentials_take_precedence_over_env() {
    let (_src_dir, src_path) = setup_db(&[("a", serde_json::json!({"x": 1}))]).await;
    let (base, log) = spawn_fake_couchdb(fake_target(false));
    let url = base.replace("http://", "http://bob:hunter2@") + "/db";

    let output = rouchdb_cmd()
        .args(["replicate", path_str(&src_path), &url])
        .env("ROUCHDB_USER", "alice")
        .env("ROUCHDB_PASSWORD", "other")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr_str(&output));

    let expected = format!("Basic {}", b64(b"bob:hunter2"));
    for req in log.lock().unwrap().iter() {
        assert_eq!(req.header("authorization"), Some(expected.as_str()));
    }
}

#[ignore]
#[tokio::test]
async fn replicate_to_couchdb_with_env_credentials() {
    let admin_url = couchdb_url();
    let db_name = format!("rouchdb_cli_env_auth_{}", std::process::id());
    let admin_db_url = format!("{}/{}", admin_url, db_name);
    // The same URL without credentials; they come from the environment.
    let (scheme, rest) = admin_url.split_once("://").unwrap();
    let (userinfo, host) = rest.rsplit_once('@').expect("COUCHDB_URL has credentials");
    let (user, password) = userinfo.split_once(':').unwrap();
    let plain_db_url = format!("{}://{}/{}", scheme, host, db_name);

    // Database::http does not create the remote database; do it up front.
    let (status, body) = couch_request("PUT", &admin_db_url, None);
    assert!(status == 201 || status == 202, "{} {}", status, body);

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
        .env("ROUCHDB_USER", user)
        .env("ROUCHDB_PASSWORD", password)
        .output()
        .unwrap();
    couch_request("DELETE", &admin_db_url, None);

    assert_eq!(without_env.status.code(), Some(1));
    assert!(stderr_str(&without_env).contains("unauthorized"));
    assert!(with_env.status.success(), "{}", stderr_str(&with_env));
    let v = stdout_json(&with_env);
    assert_eq!(v["ok"], true);
    assert_eq!(v["docs_written"], 2);
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

    let output = child.wait_with_output().unwrap();
    let stderr = stderr_str(&output);
    assert!(!stderr.contains("panicked"), "stderr: {}", stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr);
}
