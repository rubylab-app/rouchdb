//! Plugins also see writes that do not go through `Database::bulk_docs`:
//! replication into the database and attachment updates.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rouchdb::{Database, DocResult, Document, Plugin, ReplicationOptions, Result, RouchError};

/// Rejects documents with `"bad": true`, like a validate_doc_update.
struct RejectBad;

#[async_trait::async_trait]
impl Plugin for RejectBad {
    fn name(&self) -> &str {
        "reject-bad"
    }

    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        if docs.iter().any(|d| d.data.get("bad").is_some()) {
            return Err(RouchError::BadRequest("bad docs are not allowed".into()));
        }
        Ok(())
    }
}

/// Counts successful writes reported to `after_write`.
#[derive(Default)]
struct CountWrites(AtomicU64);

#[async_trait::async_trait]
impl Plugin for CountWrites {
    fn name(&self) -> &str {
        "count-writes"
    }

    async fn after_write(&self, results: &[DocResult]) -> Result<()> {
        let ok = results.iter().filter(|r| r.ok).count() as u64;
        self.0.fetch_add(ok, Ordering::SeqCst);
        Ok(())
    }
}

async fn source_with_one_bad_doc() -> Database {
    let source = Database::memory("source");
    source
        .put("good1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    source
        .put("bad", serde_json::json!({"bad": true}))
        .await
        .unwrap();
    source
        .put("good2", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    source
}

#[tokio::test]
async fn replication_runs_the_target_plugins() {
    let source = source_with_one_bad_doc().await;
    let counter = Arc::new(CountWrites::default());
    let target = Database::memory("target")
        .with_plugin(Arc::new(RejectBad))
        .with_plugin(counter.clone());

    let result = target.replicate_from(&source).await.unwrap();

    // The rejected doc is reported as denied; the others still replicate.
    assert!(!result.ok);
    assert!(
        result.errors.iter().any(|e| e.contains("bad")),
        "{:?}",
        result.errors
    );
    assert_eq!(result.docs_written, 2);
    assert!(target.get("good1").await.is_ok());
    assert!(target.get("good2").await.is_ok());
    assert!(target.get("bad").await.is_err());
    assert_eq!(counter.0.load(Ordering::SeqCst), 2);

    // The denied doc does not stall later runs.
    let again = target.replicate_from(&source).await.unwrap();
    assert!(again.ok, "{:?}", again.errors);
    assert_eq!(again.docs_read, 0);
}

#[tokio::test]
async fn push_and_live_replication_run_the_target_plugins() {
    let source = source_with_one_bad_doc().await;
    let target = Database::memory("target").with_plugin(Arc::new(RejectBad));
    source
        .replicate_to_with_opts(&target, ReplicationOptions::default())
        .await
        .unwrap();
    assert!(target.get("good1").await.is_ok());
    assert!(target.get("bad").await.is_err());

    let target = Database::memory("target2").with_plugin(Arc::new(RejectBad));
    let (mut rx, handle) = source.replicate_to_live(
        &target,
        ReplicationOptions {
            live: true,
            poll_interval: std::time::Duration::from_millis(20),
            ..Default::default()
        },
    );
    let paused = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            if matches!(event, rouchdb::ReplicationEvent::Paused) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    handle.cancel();
    assert!(paused);
    assert!(target.get("good2").await.is_ok());
    assert!(target.get("bad").await.is_err());
}

#[tokio::test]
async fn attachment_writes_reach_after_write() {
    let counter = Arc::new(CountWrites::default());
    let db = Database::memory("test").with_plugin(counter.clone());
    let rev = db
        .put("doc", serde_json::json!({}))
        .await
        .unwrap()
        .rev
        .unwrap();
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);

    let put = db
        .put_attachment("doc", "a.txt", &rev, b"hi".to_vec(), "text/plain")
        .await
        .unwrap();
    assert_eq!(counter.0.load(Ordering::SeqCst), 2);

    db.remove_attachment("doc", "a.txt", put.rev.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(counter.0.load(Ordering::SeqCst), 3);
}
