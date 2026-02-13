use std::path::PathBuf;

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

#[allow(deprecated)]
fn rouchdb_cmd() -> Command {
    Command::cargo_bin("rouchdb").unwrap()
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
