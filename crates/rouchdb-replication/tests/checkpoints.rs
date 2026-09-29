//! Checkpoint recovery across replication sessions: where a run resumes
//! after the source is replaced, a checkpoint is rolled back, a filter
//! changes, or checkpoints are disabled.

use rouchdb_adapter_memory::MemoryAdapter;
use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::{AllDocsOptions, BulkDocsOptions, Document, Seq};
use rouchdb_core::error::RouchError;
use rouchdb_replication::{Checkpointer, ReplicationFilter, ReplicationOptions, replicate};

async fn put(db: &MemoryAdapter, id: &str, data: serde_json::Value) {
    let mut json = data;
    json["_id"] = serde_json::json!(id);
    let res = db
        .bulk_docs(
            vec![Document::from_json(json).unwrap()],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
    assert!(res[0].ok, "{res:?}");
}

async fn ids(db: &MemoryAdapter) -> Vec<String> {
    db.all_docs(AllDocsOptions::new())
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r.key)
        .collect()
}

async fn rep_id(source: &MemoryAdapter, target: &MemoryAdapter) -> String {
    Checkpointer::new(
        &source.id().await.unwrap(),
        &target.id().await.unwrap(),
        "nofilter",
    )
    .replication_id()
    .to_string()
}

#[tokio::test]
async fn checkpoint_is_written_to_both_sides_with_the_final_seq() {
    let source = MemoryAdapter::new("a");
    let target = MemoryAdapter::new("b");
    for i in 0..5 {
        put(&source, &format!("d{i}"), serde_json::json!({})).await;
    }

    let result = replicate(
        &source,
        &target,
        ReplicationOptions {
            batch_size: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(result.last_seq, Seq::Num(5));

    let id = rep_id(&source, &target).await;
    let on_source = source.get_local(&id).await.unwrap();
    let on_target = target.get_local(&id).await.unwrap();
    for cp in [&on_source, &on_target] {
        assert_eq!(cp["last_seq"], 5);
        assert_eq!(cp["history"][0]["last_seq"], 5);
        assert_eq!(cp["history"][0]["session_id"], cp["session_id"]);
    }
    assert_eq!(on_source["session_id"], on_target["session_id"]);
}

#[tokio::test]
async fn replaced_source_with_the_same_name_is_rescanned() {
    let target = MemoryAdapter::new("target");
    {
        let old = MemoryAdapter::new("src");
        for i in 0..5 {
            put(&old, &format!("old{i}"), serde_json::json!({})).await;
        }
        replicate(&old, &target, ReplicationOptions::default())
            .await
            .unwrap();
    }

    // Same name, so the same replication id, but a new feed (seqs 1-2) that
    // the target's checkpoint at seq 5 knows nothing about.
    let source = MemoryAdapter::new("src");
    put(&source, "x", serde_json::json!({})).await;
    put(&source, "y", serde_json::json!({})).await;
    let result = replicate(&source, &target, ReplicationOptions::default())
        .await
        .unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!((result.docs_read, result.docs_written), (2, 2));
    assert_eq!(
        ids(&target).await,
        vec!["old0", "old1", "old2", "old3", "old4", "x", "y"]
    );
}

#[tokio::test]
async fn rolled_back_target_resumes_from_the_common_session() {
    let source = MemoryAdapter::new("a");
    let target = MemoryAdapter::new("b");
    for i in 0..5 {
        put(&source, &format!("x{i}"), serde_json::json!({})).await;
    }
    replicate(&source, &target, ReplicationOptions::default())
        .await
        .unwrap();
    let id = rep_id(&source, &target).await;
    let backup = target.get_local(&id).await.unwrap();

    for i in 0..3 {
        put(&source, &format!("y{i}"), serde_json::json!({})).await;
    }
    replicate(&source, &target, ReplicationOptions::default())
        .await
        .unwrap();

    // The target's checkpoint is restored from the backup of session 1; the
    // source's history still records session 1 at seq 5.
    target.put_local(&id, backup).await.unwrap();
    put(&source, "z", serde_json::json!({})).await;
    let result = replicate(&source, &target, ReplicationOptions::default())
        .await
        .unwrap();
    assert!(result.ok, "{:?}", result.errors);
    // Resumes at seq 5: y0-y2 are re-read (already there) and z is new.
    assert_eq!((result.docs_read, result.docs_written), (4, 1));
}

#[tokio::test]
async fn same_session_rollback_resumes_from_the_smaller_seq() {
    let source = MemoryAdapter::new("a");
    let target = MemoryAdapter::new("b");
    for i in 0..6 {
        put(&source, &format!("d{i}"), serde_json::json!({})).await;
    }
    replicate(&source, &target, ReplicationOptions::default())
        .await
        .unwrap();

    // Target restored from a backup taken mid-session, at seq 2.
    let id = rep_id(&source, &target).await;
    let mut cp = target.get_local(&id).await.unwrap();
    cp["last_seq"] = serde_json::json!(2);
    cp["history"][0]["last_seq"] = serde_json::json!(2);
    target.put_local(&id, cp).await.unwrap();

    let result = replicate(&source, &target, ReplicationOptions::default())
        .await
        .unwrap();
    assert_eq!((result.docs_read, result.docs_written), (4, 0));
}

#[tokio::test]
async fn filtered_run_does_not_make_an_unfiltered_one_skip_docs() {
    let source = MemoryAdapter::new("src");
    for (id, kind) in [("i1", "invoice"), ("u1", "user"), ("i2", "invoice")] {
        put(&source, id, serde_json::json!({"type": kind})).await;
    }

    for filter in [
        ReplicationFilter::Selector(serde_json::json!({"type": "invoice"})),
        ReplicationFilter::DocIds(vec!["i1".into(), "i2".into()]),
    ] {
        let target = MemoryAdapter::new("dst");
        let filtered = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: Some(filter),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(filtered.ok, "{:?}", filtered.errors);
        assert_eq!(ids(&target).await, vec!["i1", "i2"]);

        // A distinct checkpoint: the unfiltered run starts from zero.
        let all = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert_eq!((all.docs_read, all.docs_written), (3, 1));
        assert_eq!(ids(&target).await, vec!["i1", "i2", "u1"]);
    }
}

#[tokio::test]
async fn checkpoint_false_stores_no_checkpoint_on_either_side() {
    let source = MemoryAdapter::new("a");
    let target = MemoryAdapter::new("b");
    put(&source, "d", serde_json::json!({})).await;

    let opts = || ReplicationOptions {
        checkpoint: false,
        ..Default::default()
    };
    let first = replicate(&source, &target, opts()).await.unwrap();
    assert_eq!((first.docs_read, first.docs_written), (1, 1));

    let id = rep_id(&source, &target).await;
    assert!(matches!(
        source.get_local(&id).await,
        Err(RouchError::NotFound(_))
    ));
    assert!(matches!(
        target.get_local(&id).await,
        Err(RouchError::NotFound(_))
    ));

    // So the next run reads the whole feed again (writing nothing new).
    let second = replicate(&source, &target, opts()).await.unwrap();
    assert_eq!((second.docs_read, second.docs_written), (1, 0));
}
