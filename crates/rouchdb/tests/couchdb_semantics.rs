//! CouchDB parity tests for replication, the changes feed and the HTTP
//! adapter, run against a real CouchDB (see `common`).

mod common;

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::{BulkDocsOptions, Database, Document, GetOptions};

/// Write one revision with its full ancestry (`new_edits=false`).
async fn put_rev(db: &Database, id: &str, ids: &[&str], data: serde_json::Value) {
    let start = ids.len() as u64;
    let mut json = data;
    json["_id"] = serde_json::json!(id);
    json["_rev"] = serde_json::json!(format!("{}-{}", start, ids[0]));
    json["_revisions"] = serde_json::json!({"start": start, "ids": ids});
    let doc = Document::from_json(json).unwrap();
    db.adapter()
        .bulk_docs(vec![doc], BulkDocsOptions::replication())
        .await
        .unwrap();
}

async fn conflicts_of(db: &Database, id: &str) -> Vec<String> {
    let doc = db
        .get_with_opts(
            id,
            GetOptions {
                conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    doc.data["_conflicts"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

// =========================================================================
// Conflict branches (F12)
// =========================================================================

#[tokio::test]
#[ignore]
async fn pull_from_couchdb_brings_conflict_branches() {
    let url = fresh_remote_db("conflict_pull").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    put_rev(&remote, "d", &["bbb", "aaa"], serde_json::json!({"v": "b"})).await;
    put_rev(&remote, "d", &["ccc", "aaa"], serde_json::json!({"v": "c"})).await;
    assert_eq!(conflicts_of(&remote, "d").await, vec!["2-bbb"]);

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(conflicts_of(&local, "d").await, vec!["2-bbb"]);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn push_to_couchdb_carries_conflict_branches() {
    let url = fresh_remote_db("conflict_push").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    put_rev(&local, "d", &["bbb", "aaa"], serde_json::json!({"v": "b"})).await;
    put_rev(&local, "d", &["ccc", "aaa"], serde_json::json!({"v": "c"})).await;

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(conflicts_of(&remote, "d").await, vec!["2-bbb"]);

    delete_remote_db(&url).await;
}

// =========================================================================
// Attachments through _bulk_get (F04)
// =========================================================================

/// Create `doc1` with a `hi.txt` attachment directly in CouchDB.
async fn couch_doc_with_attachment(url: &str) -> String {
    let resp: serde_json::Value = reqwest::Client::new()
        .put(format!("{}/doc1", url))
        .json(&serde_json::json!({
            "v": 1,
            "_attachments": {"hi.txt": {"content_type": "text/plain", "data": "aGkh"}}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["rev"].as_str().unwrap().to_string()
}

#[tokio::test]
#[ignore]
async fn http_bulk_get_returns_attachment_bytes() {
    let url = fresh_remote_db("bulk_get_atts").await;
    let rev = couch_doc_with_attachment(&url).await;
    let remote = Database::http(&url);

    let resp = remote
        .adapter()
        .bulk_get(vec![rouchdb::BulkGetItem {
            id: "doc1".into(),
            rev: Some(rev),
        }])
        .await
        .unwrap();
    let doc = resp.results[0].docs[0].ok.clone().unwrap();
    assert_eq!(doc["_attachments"]["hi.txt"]["data"], "aGkh");

    let parsed = Document::from_json(doc).unwrap();
    assert_eq!(
        parsed.attachments["hi.txt"].data.as_deref(),
        Some(&b"hi!"[..])
    );

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn pull_from_couchdb_carries_attachment_bytes() {
    let url = fresh_remote_db("pull_atts").await;
    couch_doc_with_attachment(&url).await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(result.docs_written, 1);
    assert_eq!(
        local.get_attachment("doc1", "hi.txt").await.unwrap(),
        b"hi!"
    );

    delete_remote_db(&url).await;
}

// =========================================================================
// Docs rejected by validate_doc_update (F36)
// =========================================================================

#[tokio::test]
#[ignore]
async fn push_skips_docs_denied_by_validate_doc_update() {
    let url = fresh_remote_db("vdu_denied").await;
    reqwest::Client::new()
        .put(format!("{}/_design/guard", url))
        .json(&serde_json::json!({
            "validate_doc_update":
                "function(doc) { if (doc.bad) { throw({forbidden: 'no bad docs'}); } }"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let remote = Database::http(&url);
    let local = Database::memory("local");
    local.put("a", serde_json::json!({"v": 1})).await.unwrap();
    local
        .put("x", serde_json::json!({"bad": true}))
        .await
        .unwrap();
    local.put("b", serde_json::json!({"v": 3})).await.unwrap();
    let opts = || rouchdb::ReplicationOptions {
        batch_size: 1,
        ..Default::default()
    };

    let r1 = local.replicate_to_with_opts(&remote, opts()).await.unwrap();
    assert!(!r1.ok);
    assert_eq!(r1.docs_written, 2);
    assert!(
        r1.errors.iter().any(|e| e.contains("forbidden")),
        "{:?}",
        r1.errors
    );
    assert!(remote.get("b").await.is_ok());
    assert!(remote.get("x").await.is_err());

    // The denied doc is not retried on every run.
    let r2 = local.replicate_to_with_opts(&remote, opts()).await.unwrap();
    assert!(r2.ok, "{:?}", r2.errors);
    assert_eq!(r2.docs_read, 0);

    delete_remote_db(&url).await;
}

// =========================================================================
// Replication ids (F98)
// =========================================================================

#[tokio::test]
#[ignore]
async fn http_id_is_server_uuid_plus_db_name() {
    let url = fresh_remote_db("repl_id").await;
    let db_name = url.rsplit('/').next().unwrap().to_string();
    let root: serde_json::Value = reqwest::get(common::couchdb_url())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let uuid = root["uuid"].as_str().unwrap();

    let id = Database::http(&url).adapter().id().await.unwrap();
    assert_eq!(id, format!("{}{}", uuid, db_name));
    // Another URL for the same database maps to the same id.
    let alias = url.replace("localhost", "127.0.0.1");
    assert_eq!(Database::http(&alias).adapter().id().await.unwrap(), id);

    delete_remote_db(&url).await;
}

// =========================================================================
// HTTP parity: conflicts in _changes, update_seq, remote db creation (F88)
// =========================================================================

#[tokio::test]
#[ignore]
async fn http_changes_report_conflicts() {
    let url = fresh_remote_db("changes_conflicts").await;
    let remote = Database::http(&url);
    put_rev(&remote, "d", &["bbb", "aaa"], serde_json::json!({})).await;
    put_rev(&remote, "d", &["ccc", "aaa"], serde_json::json!({})).await;

    for include_docs in [false, true] {
        let changes = remote
            .changes(rouchdb::ChangesOptions {
                conflicts: true,
                include_docs,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            changes.results[0].conflicts,
            Some(vec!["2-bbb".to_string()]),
            "include_docs={include_docs}"
        );
        assert_eq!(changes.results[0].doc.is_some(), include_docs);
    }

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn http_all_docs_reports_update_seq() {
    let url = fresh_remote_db("all_docs_seq").await;
    let remote = Database::http(&url);
    remote.put("a", serde_json::json!({})).await.unwrap();

    let resp = remote
        .all_docs(rouchdb::AllDocsOptions {
            update_seq: true,
            ..rouchdb::AllDocsOptions::new()
        })
        .await
        .unwrap();
    let seq = resp.update_seq.expect("update_seq requested");
    assert_eq!(seq.as_num(), 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn http_creates_missing_database_on_first_use() {
    let url = format!(
        "{}/created_on_use_{}",
        common::couchdb_url(),
        uuid::Uuid::new_v4().simple()
    );
    let remote = Database::http(&url);

    let local = Database::memory("local");
    local.put("a", serde_json::json!({"v": 1})).await.unwrap();
    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(remote.get("a").await.unwrap().data["v"], 1);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn http_skip_setup_does_not_create_database() {
    let url = format!(
        "{}/not_created_{}",
        common::couchdb_url(),
        uuid::Uuid::new_v4().simple()
    );
    let remote =
        rouchdb::HttpAdapter::with_options(&url, rouchdb::HttpAdapterOptions { skip_setup: true });
    use rouchdb::Adapter;
    assert!(matches!(
        remote.info().await,
        Err(rouchdb::RouchError::NotFound(_))
    ));
}

// =========================================================================
// HTTP error mapping (F90)
// =========================================================================

#[tokio::test]
#[ignore]
async fn http_errors_keep_couchdb_meaning() {
    let url = fresh_remote_db("http_errors").await;
    let remote = Database::http(&url);
    remote.put("d", serde_json::json!({})).await.unwrap();

    let err = remote
        .get_with_opts(
            "d",
            GetOptions {
                rev: Some("not-a-rev".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, rouchdb::RouchError::BadRequest(_)), "{err:?}");

    let server = common::couchdb_url().replace("admin:password@", "");
    let auth = rouchdb::AuthClient::new(&server);
    let err = auth.login("admin", "wrong-password").await.unwrap_err();
    assert!(matches!(err, rouchdb::RouchError::Unauthorized), "{err:?}");

    delete_remote_db(&url).await;
}
