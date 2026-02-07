use std::collections::HashMap;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::Result;

use crate::checkpoint::Checkpointer;

/// Replication configuration.
#[derive(Debug, Clone)]
pub struct ReplicationOptions {
    /// Number of documents to process per batch.
    pub batch_size: u64,
    /// Maximum number of batches to buffer.
    pub batches_limit: u64,
}

impl Default for ReplicationOptions {
    fn default() -> Self {
        Self {
            batch_size: 100,
            batches_limit: 10,
        }
    }
}

/// Result of a completed replication.
#[derive(Debug, Clone)]
pub struct ReplicationResult {
    pub ok: bool,
    pub docs_read: u64,
    pub docs_written: u64,
    pub errors: Vec<String>,
    pub last_seq: u64,
}

/// Events emitted during replication for progress tracking.
#[derive(Debug, Clone)]
pub enum ReplicationEvent {
    Change { docs_read: u64 },
    Paused,
    Active,
    Complete(ReplicationResult),
    Error(String),
}

/// Run a one-shot replication from source to target.
///
/// Implements the CouchDB replication protocol:
/// 1. Read checkpoint
/// 2. Fetch changes from source
/// 3. Compute revs_diff against target
/// 4. Fetch missing docs from source
/// 5. Write to target
/// 6. Save checkpoint
pub async fn replicate(
    source: &dyn Adapter,
    target: &dyn Adapter,
    opts: ReplicationOptions,
) -> Result<ReplicationResult> {
    let source_info = source.info().await?;
    let target_info = target.info().await?;

    let checkpointer = Checkpointer::new(&source_info.db_name, &target_info.db_name);

    // Step 1: Read checkpoint
    let since = checkpointer.read_checkpoint(source, target).await?;

    let mut total_docs_read = 0u64;
    let mut total_docs_written = 0u64;
    let mut errors = Vec::new();
    let mut current_seq = since;

    loop {
        // Step 2: Fetch changes from source
        let changes = source
            .changes(ChangesOptions {
                since: current_seq,
                limit: Some(opts.batch_size),
                include_docs: false,
                ..Default::default()
            })
            .await?;

        if changes.results.is_empty() {
            break; // No more changes
        }

        let batch_last_seq = changes.last_seq;
        total_docs_read += changes.results.len() as u64;

        // Step 3: Compute revision diff
        let mut rev_map: HashMap<String, Vec<String>> = HashMap::new();
        for change in &changes.results {
            let revs: Vec<String> = change.changes.iter().map(|c| c.rev.clone()).collect();
            rev_map.insert(change.id.clone(), revs);
        }

        let diff = target.revs_diff(rev_map).await?;

        if diff.results.is_empty() {
            // Target already has everything in this batch
            current_seq = batch_last_seq;
            continue;
        }

        // Step 4: Fetch missing documents from source
        let mut bulk_get_items: Vec<BulkGetItem> = Vec::new();
        for (doc_id, diff_result) in &diff.results {
            for missing_rev in &diff_result.missing {
                bulk_get_items.push(BulkGetItem {
                    id: doc_id.clone(),
                    rev: Some(missing_rev.clone()),
                });
            }
        }

        let bulk_get_response = source.bulk_get(bulk_get_items).await?;

        // Step 5: Write to target with new_edits=false
        let mut docs_to_write: Vec<Document> = Vec::new();
        for result in &bulk_get_response.results {
            for doc in &result.docs {
                if let Some(ref json) = doc.ok {
                    match Document::from_json(json.clone()) {
                        Ok(document) => docs_to_write.push(document),
                        Err(e) => errors.push(format!("parse error for {}: {}", result.id, e)),
                    }
                }
            }
        }

        if !docs_to_write.is_empty() {
            let write_count = docs_to_write.len() as u64;
            let write_results = target
                .bulk_docs(docs_to_write, BulkDocsOptions::replication())
                .await?;

            for wr in &write_results {
                if !wr.ok {
                    errors.push(format!(
                        "write error for {}: {}",
                        wr.id,
                        wr.reason.as_deref().unwrap_or("unknown")
                    ));
                }
            }

            total_docs_written += write_count;
        }

        // Step 6: Save checkpoint
        current_seq = batch_last_seq;
        let _ = checkpointer
            .write_checkpoint(source, target, current_seq)
            .await;

        // Check if we got fewer results than batch_size (last batch)
        if (changes.results.len() as u64) < opts.batch_size {
            break;
        }
    }

    Ok(ReplicationResult {
        ok: errors.is_empty(),
        docs_read: total_docs_read,
        docs_written: total_docs_written,
        errors,
        last_seq: current_seq,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rouchdb_adapter_memory::MemoryAdapter;

    async fn put_doc(adapter: &dyn Adapter, id: &str, data: serde_json::Value) {
        let doc = Document {
            id: id.into(),
            rev: None,
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        adapter
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn replicate_empty_databases() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_read, 0);
        assert_eq!(result.docs_written, 0);
    }

    #[tokio::test]
    async fn replicate_source_to_target() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        put_doc(&source, "doc1", serde_json::json!({"name": "Alice"})).await;
        put_doc(&source, "doc2", serde_json::json!({"name": "Bob"})).await;
        put_doc(&source, "doc3", serde_json::json!({"name": "Charlie"})).await;

        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_read, 3);
        assert_eq!(result.docs_written, 3);

        // Verify target has the documents
        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 3);

        let doc = target.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(doc.data["name"], "Alice");
    }

    #[tokio::test]
    async fn replicate_incremental() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        // First replication
        put_doc(&source, "doc1", serde_json::json!({"v": 1})).await;
        let r1 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert_eq!(r1.docs_written, 1);

        // Add more docs
        put_doc(&source, "doc2", serde_json::json!({"v": 2})).await;
        put_doc(&source, "doc3", serde_json::json!({"v": 3})).await;

        // Second replication should only sync new docs
        let r2 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert_eq!(r2.docs_read, 2);
        assert_eq!(r2.docs_written, 2);

        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 3);
    }

    #[tokio::test]
    async fn replicate_already_synced() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        put_doc(&source, "doc1", serde_json::json!({"v": 1})).await;

        // First replication
        replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        // Second replication with no new changes
        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_written, 0);
    }

    #[tokio::test]
    async fn replicate_batched() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        // Create more docs than batch size
        for i in 0..15 {
            put_doc(&source, &format!("doc{:03}", i), serde_json::json!({"i": i})).await;
        }

        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                batch_size: 5,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_written, 15);

        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 15);
    }

    #[tokio::test]
    async fn replicate_with_deletes() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        // Create and sync
        put_doc(&source, "doc1", serde_json::json!({"v": 1})).await;
        replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        // Delete on source
        let doc = source.get("doc1", GetOptions::default()).await.unwrap();
        let del = Document {
            id: "doc1".into(),
            rev: doc.rev,
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        source
            .bulk_docs(vec![del], BulkDocsOptions::new())
            .await
            .unwrap();

        // Replicate delete
        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(result.ok);

        // Target should see deletion
        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 0);
    }
}
