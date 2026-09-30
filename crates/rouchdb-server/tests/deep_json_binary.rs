//! The `rouchdb-server` binary serves documents, selectors and Mango index
//! keys nested `MAX_NESTING_DEPTH` levels deep without running out of stack,
//! and starts with an index over such values.
//!
//! Parsing, matching, sorting and serializing walk these values
//! recursively, so how deep they can go depends on the stacks of the
//! threads that do it. Unlike the in-process tests (`deep_json.rs`, which
//! run the router on the test's thread), these run the real binary, whose
//! `main` decides those stacks: the thread that restores the indexes at
//! startup and the runtime's worker and blocking threads. By default they
//! are 1 MiB (the main thread on Windows) and 2 MiB (every tokio thread),
//! which a selector of nested objects this deep overflows in a debug build
//! even on Linux. CI runs these tests on Windows too.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rouchdb::MAX_NESTING_DEPTH;
use serde_json::{Value, json};

/// `{<members>"v": [[...[1]...]]}`: `depth` containers deep counting the
/// document itself. Built as text: a `Value` this deep would overflow the
/// test's own stack when serialized or dropped.
fn deep_array_doc(depth: usize, members: &str) -> String {
    format!(
        r#"{{{members}"v":{}1{}}}"#,
        "[".repeat(depth - 1),
        "]".repeat(depth - 1)
    )
}

/// `{<members>"v": {"a": {"a": ... 1}}}`, `depth` containers deep. As a
/// selector, every level is one more path segment (`v.a.a...`).
fn deep_object_doc(depth: usize, members: &str) -> String {
    format!(
        r#"{{{members}"v":{}1{}}}"#,
        r#"{"a":"#.repeat(depth - 1),
        "}".repeat(depth - 1)
    )
}

/// The value of `v` in a document built by the functions above, as the
/// server writes it back (compact JSON).
fn v_of(doc: &str) -> &str {
    let start = doc.find(r#""v":"#).unwrap() + 4;
    &doc[start..doc.len() - 1]
}

/// A running `rouchdb-server` process, killed on drop.
struct Server {
    child: Child,
    url: String,
    stderr: Arc<Mutex<String>>,
    client: reqwest::Client,
}

impl Server {
    /// Start the binary on `path` (database `db`) on a free port.
    fn start(path: &Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rouchdb-server"))
            .arg(path)
            .args(["--port", "0", "--db-name", "db"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stderr = Arc::new(Mutex::new(String::new()));
        let mut pipe = child.stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });

        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(url) = line.strip_prefix("RouchDB server listening on ") {
                    let _ = tx.send(url.to_string());
                }
            }
        });

        let mut server = Server {
            child,
            url: String::new(),
            stderr,
            client: reqwest::Client::new(),
        };
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(url) => server.url = url,
            Err(_) => panic!("the server did not start: {}", server.died()),
        }
        server
    }

    /// What became of the process, with its stderr (e.g. "thread 'main'
    /// has overflowed its stack").
    fn died(&mut self) -> String {
        // Give a crashing process a moment to exit and flush stderr.
        let mut status = None;
        for _ in 0..50 {
            status = self.child.try_wait().unwrap();
            if status.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(200));
        let status = match status {
            Some(status) => format!("exited with {status}"),
            None => "still running".to_string(),
        };
        format!("{status}; stderr:\n{}", self.stderr.lock().unwrap())
    }

    /// Send a request; `body` is JSON text. Returns the status and the body.
    async fn call(&mut self, method: &str, path: &str, body: Option<String>) -> (u16, String) {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = self
            .client
            .request(method.clone(), format!("{}/db{path}", self.url))
            .timeout(Duration::from_secs(120));
        if let Some(body) = body {
            req = req.header("content-type", "application/json").body(body);
        }
        let result = match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                resp.text().await.map(|text| (status, text))
            }
            Err(e) => Err(e),
        };
        result.unwrap_or_else(|e| panic!("{method} /db{path} failed ({e}): {}", self.died()))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ids(body: &str) -> Vec<String> {
    let body: Value = serde_json::from_str(body).unwrap();
    let mut ids: Vec<String> = body["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn documents_and_selectors_up_to_the_limit_are_served() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::start(&dir.path().join("db.redb"));
    let max = MAX_NESTING_DEPTH;

    for (shape, doc) in [
        ("array", deep_array_doc as fn(usize, &str) -> String),
        ("object", deep_object_doc),
    ] {
        let edge = doc(max, "");
        let id = |name: &str| format!("{shape}-{name}");

        // Every way to write one.
        let (status, body) = server
            .call("PUT", &format!("/{}", id("put")), Some(edge.clone()))
            .await;
        assert_eq!(status, 201, "{shape}: {body}");
        let posted = doc(max, &format!(r#""_id":"{}","#, id("posted")));
        let (status, body) = server.call("POST", "", Some(posted)).await;
        assert_eq!(status, 201, "{shape}: {body}");
        let bulk = format!(
            r#"{{"docs":[{}]}}"#,
            doc(max, &format!(r#""_id":"{}","#, id("bulk")))
        );
        let (status, body) = server.call("POST", "/_bulk_docs", Some(bulk)).await;
        assert_eq!(status, 201, "{shape}: {body}");
        let (status, body) = server
            .call("PUT", &format!("/_local/{shape}"), Some(edge.clone()))
            .await;
        assert_eq!(status, 201, "{shape}: {body}");

        // Every way to read them back.
        for name in ["put", "posted", "bulk"] {
            let (status, body) = server.call("GET", &format!("/{}", id(name)), None).await;
            assert_eq!(status, 200, "{shape} {name}");
            assert!(body.contains(v_of(&edge)), "{shape} {name}");
        }
        let (status, body) = server.call("GET", &format!("/_local/{shape}"), None).await;
        assert_eq!(status, 200, "{shape}");
        assert!(body.contains(v_of(&edge)), "{shape}");
        for path in [
            "/_all_docs?include_docs=true",
            "/_changes?include_docs=true&style=all_docs",
        ] {
            let (status, body) = server.call("GET", path, None).await;
            assert_eq!(status, 200, "{shape} {path}");
            assert_eq!(body.matches(v_of(&edge)).count(), 3, "{shape} {path}");
        }
        let request = format!(r#"{{"docs":[{{"id":"{}"}}]}}"#, id("bulk"));
        let (status, body) = server
            .call("POST", "/_bulk_get?revs=true", Some(request))
            .await;
        assert_eq!(status, 200, "{shape}");
        assert!(body.contains(v_of(&edge)), "{shape}");

        // A selector as deep as the documents finds them.
        let query = format!(r#"{{"selector":{edge},"fields":["_id"]}}"#);
        let (status, body) = server.call("POST", "/_find", Some(query)).await;
        assert_eq!(status, 200, "{shape}: {body}");
        assert_eq!(ids(&body), [id("bulk"), id("posted"), id("put")], "{shape}");

        // Deeper documents are refused, not a crash.
        let (status, body) = server.call("PUT", "/deeper", Some(doc(5 * max, ""))).await;
        assert_eq!(status, 400, "{shape}: {body}");
        assert!(body.contains("Document nesting exceeds"), "{shape}: {body}");
    }

    // A Mango index whose keys are the deep values: building it sorts them.
    let index = r#"{"index":{"fields":["v"]},"ddoc":"by-v","name":"by-v"}"#;
    let (status, body) = server.call("POST", "/_index", Some(index.into())).await;
    assert_eq!(status, 200, "{body}");
    let query = r#"{"selector":{"v":{"$gt":null}},"sort":["v"],"fields":["_id"]}"#;
    let (status, body) = server.call("POST", "/_find", Some(query.into())).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body).len(), 6);
}

/// Write `docs` and a Mango index on `v` into the database at `path`,
/// in-process on a thread with a large stack, so that only the binary's
/// startup is under test.
fn seed_with_index(path: &Path, docs: Vec<String>) {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const STACK: usize = 256 << 20;
    let path = path.to_path_buf();
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .thread_stack_size(STACK)
                .build()
                .unwrap();
            runtime.block_on(async move {
                let db = Arc::new(rouchdb::Database::open(&path, "db").unwrap());
                let config = rouchdb_server::ServerConfig {
                    db_name: "db".into(),
                    ..Default::default()
                };
                let app = rouchdb_server::build_router(db, &config);
                let index = r#"{"index":{"fields":["v"]},"ddoc":"by-v","name":"by-v"}"#;
                let requests = docs
                    .into_iter()
                    .enumerate()
                    .map(|(i, doc)| ("PUT", format!("/db/doc{i}"), doc))
                    .chain([("POST", "/db/_index".to_string(), index.to_string())]);
                for (method, uri, body) in requests {
                    let req = Request::builder()
                        .method(method)
                        .uri(&uri)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap();
                    let resp = app.clone().oneshot(req).await.unwrap();
                    let status = resp.status();
                    let body = resp.into_body().collect().await.unwrap().to_bytes();
                    assert!(status.is_success(), "{uri}: {status} {body:?}");
                }
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

/// At startup the server rebuilds its Mango indexes from their design
/// documents, on the thread that runs `main`, before it listens: that
/// sorts the indexed values.
#[tokio::test]
async fn starts_with_an_index_on_deep_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.redb");
    let max = MAX_NESTING_DEPTH;
    seed_with_index(
        &path,
        vec![deep_array_doc(max, ""), deep_object_doc(max, "")],
    );

    let mut server = Server::start(&path);
    let query = json!({
        "selector": {"v": {"$gt": null}},
        "sort": ["v"],
        "fields": ["_id"],
    });
    let (status, body) = server.call("POST", "/_find", Some(query.to_string())).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&body), ["doc0", "doc1"]);
}
