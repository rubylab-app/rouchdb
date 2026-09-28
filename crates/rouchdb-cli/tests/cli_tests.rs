use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use assert_cmd::Command;
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
