//! Replication checkpoints keyed on the identity of each local database:
//! same-named databases, a redb file and its copy, destroyed and reused
//! databases (source or target), `DocIds`
//! selections that only differ in how their ids are joined, and invalid
//! selectors. Every scenario runs on the in-memory and the redb adapter.

mod backends;

use std::time::Duration;

use backends::{Backend, KINDS, backends};
use rouchdb::{
    AllDocsOptions, Database, ReplicationEvent, ReplicationFilter, ReplicationOptions, RouchError,
};

async fn ids(db: &Database) -> Vec<String> {
    db.all_docs(AllDocsOptions::new())
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r.key)
        .collect()
}

async fn has(db: &Database, id: &str) -> bool {
    db.get(id).await.is_ok()
}

fn docids(ids: &[&str]) -> ReplicationOptions {
    ReplicationOptions {
        filter: Some(ReplicationFilter::DocIds(
            ids.iter().map(|s| s.to_string()).collect(),
        )),
        ..Default::default()
    }
}

// =========================================================================
// Two local databases with the same name
// =========================================================================

#[tokio::test]
async fn sync_between_same_named_databases_copies_both_ways() {
    for kind in KINDS {
        let a = Backend::open(kind, "same");
        let b = Backend::open(kind, "same");
        a.db.put("a", serde_json::json!({"from": "A"}))
            .await
            .unwrap();
        b.db.put("b", serde_json::json!({"from": "B"}))
            .await
            .unwrap();

        let (push, pull) = a.db.sync(&b.db).await.unwrap();
        assert!(push.ok && pull.ok, "{kind}: {push:?} {pull:?}");
        assert_eq!(push.docs_written, 1, "{kind}");
        assert_eq!(pull.docs_written, 1, "{kind}: pull skipped B's document");
        assert_eq!(ids(&a.db).await, ["a", "b"], "{kind}");
        assert_eq!(ids(&b.db).await, ["a", "b"], "{kind}");

        // A retry has nothing left to do, and changes nothing.
        let (push, pull) = a.db.sync(&b.db).await.unwrap();
        assert_eq!((push.docs_written, pull.docs_written), (0, 0), "{kind}");
        assert_eq!(ids(&a.db).await, ["a", "b"], "{kind}");
    }
}

#[tokio::test]
async fn local_database_ids_differ_from_the_name_and_from_each_other() {
    for kind in KINDS {
        let a = Backend::open(kind, "same");
        let b = Backend::open(kind, "same");
        let (id_a, id_b) = (
            a.db.adapter().id().await.unwrap(),
            b.db.adapter().id().await.unwrap(),
        );
        assert_ne!(id_a, id_b, "{kind}");
        // Stable for the same handle.
        assert_eq!(a.db.adapter().id().await.unwrap(), id_a, "{kind}");
    }
}

#[tokio::test]
async fn redb_identity_survives_reopen_and_the_checkpoint_is_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("a.redb");
    let path_b = dir.path().join("b.redb");

    let a = Database::open(&path_a, "same").unwrap();
    let b = Database::open(&path_b, "same").unwrap();
    for i in 0..3 {
        a.put(&format!("a{i}"), serde_json::json!({}))
            .await
            .unwrap();
    }
    b.put("b0", serde_json::json!({})).await.unwrap();
    let (id_a, id_b) = (
        a.adapter().id().await.unwrap(),
        b.adapter().id().await.unwrap(),
    );
    let (push, pull) = a.sync(&b).await.unwrap();
    assert_eq!((push.docs_written, pull.docs_written), (3, 1));
    drop((a, b));

    let a = Database::open(&path_a, "same").unwrap();
    let b = Database::open(&path_b, "same").unwrap();
    assert_eq!(a.adapter().id().await.unwrap(), id_a);
    assert_eq!(b.adapter().id().await.unwrap(), id_b);

    // Both directions resume from their own checkpoint: only the new
    // changes are read.
    a.put("a3", serde_json::json!({})).await.unwrap();
    b.put("b1", serde_json::json!({})).await.unwrap();
    let (push, pull) = a.sync(&b).await.unwrap();
    assert_eq!(push.docs_written, 1);
    assert_eq!(pull.docs_written, 1);
    // Each side reads at most its new document plus the one the other
    // direction wrote after the checkpoint; a rescan would read 5.
    assert!(push.docs_read <= 2, "push rescanned: {push:?}");
    assert!(pull.docs_read <= 2, "pull rescanned: {pull:?}");
    assert_eq!(ids(&a).await, ids(&b).await);
    assert_eq!(ids(&a).await.len(), 6);
}

// =========================================================================
// A redb file and a copy of it
// =========================================================================

/// A redb file with one document, closed, and a copy of it: both hold the
/// same persisted uuid.
async fn file_and_copy(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let original = dir.join("original.redb");
    let copy = dir.join("copy.redb");
    let db = Database::open(&original, "db").unwrap();
    db.put("seed", serde_json::json!({})).await.unwrap();
    db.close().await.unwrap();
    drop(db);
    std::fs::copy(&original, &copy).unwrap();
    (original, copy)
}

#[tokio::test]
async fn sync_between_a_redb_file_and_its_copy_copies_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let (path_a, path_b) = file_and_copy(dir.path()).await;
    let a = Database::open(&path_a, "original").unwrap();
    let b = Database::open(&path_b, "copy").unwrap();
    assert_ne!(
        a.adapter().id().await.unwrap(),
        b.adapter().id().await.unwrap(),
        "a copy of a file must not share its identity"
    );
    a.put("a", serde_json::json!({"from": "A"})).await.unwrap();
    b.put("b", serde_json::json!({"from": "B"})).await.unwrap();

    let (push, pull) = a.sync(&b).await.unwrap();
    assert!(push.ok && pull.ok, "{push:?} {pull:?}");
    assert!(push.warnings.is_empty() && pull.warnings.is_empty());
    assert_eq!(push.docs_written, 1, "{push:?}");
    assert_eq!(
        pull.docs_written, 1,
        "pull skipped the copy's document: {pull:?}"
    );
    assert_eq!(ids(&a).await, ["a", "b", "seed"]);
    assert_eq!(ids(&b).await, ["a", "b", "seed"]);

    // A retry has nothing left to do.
    let (push, pull) = a.sync(&b).await.unwrap();
    assert_eq!((push.docs_written, pull.docs_written), (0, 0));

    // Nor after reopening both files, which keeps both identities; new
    // divergent writes still go both ways.
    let (id_a, id_b) = (
        a.adapter().id().await.unwrap(),
        b.adapter().id().await.unwrap(),
    );
    drop((a, b));
    let a = Database::open(&path_a, "original").unwrap();
    let b = Database::open(&path_b, "copy").unwrap();
    assert_eq!(a.adapter().id().await.unwrap(), id_a);
    assert_eq!(b.adapter().id().await.unwrap(), id_b);
    let (push, pull) = a.sync(&b).await.unwrap();
    assert_eq!((push.docs_written, pull.docs_written), (0, 0));
    a.put("a2", serde_json::json!({})).await.unwrap();
    b.put("b2", serde_json::json!({})).await.unwrap();
    let (push, pull) = a.sync(&b).await.unwrap();
    assert_eq!((push.docs_written, pull.docs_written), (1, 1));
    assert_eq!(ids(&a).await, ["a", "a2", "b", "b2", "seed"]);
    assert_eq!(ids(&b).await, ["a", "a2", "b", "b2", "seed"]);
    assert_eq!(
        a.get("b").await.unwrap().data["from"],
        serde_json::json!("B")
    );
    assert_eq!(
        b.get("a").await.unwrap().data["from"],
        serde_json::json!("A")
    );
}

#[tokio::test]
async fn redb_identity_is_bound_to_the_file_location() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.redb");
    let db = Database::open(&path, "db").unwrap();
    db.put("d", serde_json::json!({})).await.unwrap();
    let id = db.adapter().id().await.unwrap();
    drop(db);

    // The same file through another spelling of its path, or a symlink.
    let spelled = dir.path().join(".").join("db.redb");
    let db = Database::open(&spelled, "db").unwrap();
    assert_eq!(db.adapter().id().await.unwrap(), id);
    drop(db);
    #[cfg(unix)]
    {
        let link = dir.path().join("link.redb");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let db = Database::open(&link, "db").unwrap();
        assert_eq!(db.adapter().id().await.unwrap(), id);
        drop(db);
    }

    // A moved file is another replica: its documents are intact, and its
    // next replication with each peer rescans once.
    let moved = dir.path().join("moved.redb");
    std::fs::rename(&path, &moved).unwrap();
    let db = Database::open(&moved, "db").unwrap();
    assert_ne!(db.adapter().id().await.unwrap(), id);
    assert_eq!(ids(&db).await, ["d"]);
}

#[tokio::test]
async fn destroy_gives_a_local_database_a_new_identity() {
    for kind in KINDS {
        let db = Backend::open(kind, "db");
        let before = db.db.adapter().id().await.unwrap();
        db.db.destroy().await.unwrap();
        let after = db.db.adapter().id().await.unwrap();
        assert_ne!(before, after, "{kind}");
        assert_eq!(db.db.adapter().id().await.unwrap(), after, "{kind}");
    }

    // redb persists the new identity.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.redb");
    let db = Database::open(&path, "db").unwrap();
    db.destroy().await.unwrap();
    let renewed = db.adapter().id().await.unwrap();
    drop(db);
    let db = Database::open(&path, "db").unwrap();
    assert_eq!(db.adapter().id().await.unwrap(), renewed);
}

#[tokio::test]
async fn one_shot_replication_after_destroying_the_source_copies_the_new_documents() {
    for kind in KINDS {
        let source = Backend::open(kind, "source");
        let target = Backend::open(kind, "target");
        for i in 0..3 {
            source
                .db
                .put(&format!("old{i}"), serde_json::json!({}))
                .await
                .unwrap();
        }
        source.db.replicate_to(&target.db).await.unwrap();

        source.db.destroy().await.unwrap();
        source.db.put("fresh", serde_json::json!({})).await.unwrap();
        let result = source.db.replicate_to(&target.db).await.unwrap();
        assert_eq!(result.docs_written, 1, "{kind}: {result:?}");
        assert!(has(&target.db, "fresh").await, "{kind}");
    }
}

// =========================================================================
// Live replication from a source that is destroyed and reused
// =========================================================================

/// Receive events until `Paused`.
async fn until_paused(rx: &mut tokio::sync::mpsc::Receiver<ReplicationEvent>) {
    let wait = async {
        while let Some(event) = rx.recv().await {
            if matches!(event, ReplicationEvent::Paused) {
                return;
            }
        }
        panic!("live replication ended before pausing");
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .expect("live replication never paused");
}

/// Wait until `db` has document `id`, draining `rx` meanwhile.
async fn wait_for(
    db: &Database,
    id: &str,
    rx: &mut tokio::sync::mpsc::Receiver<ReplicationEvent>,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if has(db, id).await {
            return true;
        }
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    has(db, id).await
}

#[tokio::test]
async fn live_replication_follows_a_destroyed_and_reused_source() {
    for kind in KINDS {
        let source = Backend::open(kind, "source");
        let target = Backend::open(kind, "target");
        for i in 0..3 {
            source
                .db
                .put(&format!("old{i}"), serde_json::json!({}))
                .await
                .unwrap();
        }

        let (mut rx, handle) = source.db.replicate_to_live(
            &target.db,
            ReplicationOptions {
                poll_interval: Duration::from_millis(50),
                live: true,
                ..Default::default()
            },
        );
        until_paused(&mut rx).await;
        assert_eq!(ids(&target.db).await.len(), 3, "{kind}");

        // The reused source numbers its changes from 1 again, below the
        // session's cursor (3).
        source.db.destroy().await.unwrap();
        source.db.put("fresh", serde_json::json!({})).await.unwrap();
        assert!(
            wait_for(&target.db, "fresh", &mut rx).await,
            "{kind}: live replication skipped a document of the reused source"
        );

        // And it keeps following the reused source.
        for i in 0..4 {
            source
                .db
                .put(&format!("new{i}"), serde_json::json!({}))
                .await
                .unwrap();
        }
        assert!(wait_for(&target.db, "new3", &mut rx).await, "{kind}");
        for i in 0..4 {
            assert!(has(&target.db, &format!("new{i}")).await, "{kind}");
        }
        handle.cancel();
    }
}

#[tokio::test]
async fn live_replication_after_the_reused_source_passes_the_old_cursor() {
    for kind in KINDS {
        let source = Backend::open(kind, "source");
        let target = Backend::open(kind, "target");
        source.db.put("old", serde_json::json!({})).await.unwrap();

        let (mut rx, handle) = source.db.replicate_to_live(
            &target.db,
            ReplicationOptions {
                poll_interval: Duration::from_millis(50),
                live: true,
                ..Default::default()
            },
        );
        until_paused(&mut rx).await;

        // Several writes land before the next pass: the reused source's
        // update_seq ends above the old cursor (1).
        source.db.destroy().await.unwrap();
        for i in 0..5 {
            source
                .db
                .put(&format!("fresh{i}"), serde_json::json!({}))
                .await
                .unwrap();
        }
        assert!(wait_for(&target.db, "fresh4", &mut rx).await, "{kind}");
        for i in 0..5 {
            assert!(
                has(&target.db, &format!("fresh{i}")).await,
                "{kind}: fresh{i} skipped"
            );
        }
        handle.cancel();
    }
}

// =========================================================================
// Live replication to a target that is destroyed and reused
// =========================================================================

#[tokio::test]
async fn live_replication_refills_a_destroyed_target_while_the_source_is_idle() {
    for kind in KINDS {
        let source = Backend::open(kind, "source");
        let target = Backend::open(kind, "target");
        for i in 0..3 {
            source
                .db
                .put(&format!("keep{i}"), serde_json::json!({}))
                .await
                .unwrap();
        }

        let (mut rx, handle) = source.db.replicate_to_live(
            &target.db,
            ReplicationOptions {
                // Long enough that only the reset notice can wake the
                // session within the test's deadline.
                poll_interval: Duration::from_secs(3600),
                live: true,
                ..Default::default()
            },
        );
        until_paused(&mut rx).await;
        assert_eq!(ids(&target.db).await.len(), 3, "{kind}");

        // The source stays idle: nothing is written to it from here on.
        let source_seq = source.db.info().await.unwrap().update_seq;
        target.db.destroy().await.unwrap();
        target
            .db
            .put("target_reused", serde_json::json!({}))
            .await
            .unwrap();
        assert!(
            wait_for(&target.db, "keep2", &mut rx).await,
            "{kind}: the destroyed target was not refilled"
        );
        assert_eq!(
            ids(&target.db).await,
            ["keep0", "keep1", "keep2", "target_reused"],
            "{kind}"
        );
        assert_eq!(source.db.info().await.unwrap().update_seq, source_seq);

        // The session keeps following the source into the new target.
        source.db.put("after", serde_json::json!({})).await.unwrap();
        assert!(wait_for(&target.db, "after", &mut rx).await, "{kind}");
        handle.cancel();
    }
}

// =========================================================================
// DocIds selections
// =========================================================================

#[tokio::test]
async fn docids_selections_joined_by_nul_do_not_share_a_checkpoint() {
    // Both orders: the joined id first, then the pair, and back.
    let orders: [(&[&str], &[&str]); 2] = [(&["a", "b"], &["a\0b"]), (&["a\0b"], &["b", "a"])];
    for kind in KINDS {
        for (first, second) in orders {
            let source = Backend::open(kind, "source");
            let target = Backend::open(kind, "target");
            // The joined id gets the lowest sequence.
            source.db.put("a\0b", serde_json::json!({})).await.unwrap();
            source.db.put("a", serde_json::json!({})).await.unwrap();
            source.db.put("b", serde_json::json!({})).await.unwrap();

            let r1 = rouchdb::replicate(source.db.adapter(), target.db.adapter(), docids(first))
                .await
                .unwrap();
            assert_eq!(r1.docs_written as usize, first.len(), "{kind} {first:?}");
            let r2 = rouchdb::replicate(source.db.adapter(), target.db.adapter(), docids(second))
                .await
                .unwrap();
            assert_eq!(
                r2.docs_written as usize,
                second.len(),
                "{kind}: {second:?} reused {first:?}'s checkpoint"
            );
            assert_eq!(ids(&target.db).await, ["a", "a\0b", "b"], "{kind}");

            // Retries of either selection change nothing.
            for sel in [first, second] {
                let r = rouchdb::replicate(source.db.adapter(), target.db.adapter(), docids(sel))
                    .await
                    .unwrap();
                assert!(r.ok, "{kind}: {r:?}");
                assert_eq!(r.docs_written, 0, "{kind}");
            }
            assert_eq!(ids(&target.db).await, ["a", "a\0b", "b"], "{kind}");
        }
    }
}

// =========================================================================
// Invalid selectors
// =========================================================================

fn bad_selector() -> ReplicationOptions {
    ReplicationOptions {
        filter: Some(ReplicationFilter::Selector(
            serde_json::json!({"x": {"$typo": 1}}),
        )),
        ..Default::default()
    }
}

#[tokio::test]
async fn an_invalid_replication_selector_is_rejected_before_any_work() {
    for kind in KINDS {
        let source = Backend::open(kind, "source");
        let target = Backend::open(kind, "target");

        // Even with an empty feed.
        let err = rouchdb::replicate(source.db.adapter(), target.db.adapter(), bad_selector())
            .await
            .unwrap_err();
        assert!(matches!(err, RouchError::BadRequest(_)), "{kind}: {err:?}");

        source
            .db
            .put("d", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        let err = rouchdb::replicate(source.db.adapter(), target.db.adapter(), bad_selector())
            .await
            .unwrap_err();
        assert!(matches!(err, RouchError::BadRequest(_)), "{kind}: {err:?}");
        let err = source
            .db
            .replicate_to_with_opts(&target.db, bad_selector())
            .await
            .unwrap_err();
        assert!(matches!(err, RouchError::BadRequest(_)), "{kind}: {err:?}");

        // No checkpoint was stored: the fixed selector copies the document.
        let fixed = ReplicationOptions {
            filter: Some(ReplicationFilter::Selector(
                serde_json::json!({"x": {"$eq": 1}}),
            )),
            ..Default::default()
        };
        let ok = rouchdb::replicate(source.db.adapter(), target.db.adapter(), fixed)
            .await
            .unwrap();
        assert_eq!(ok.docs_written, 1, "{kind}");
    }
}

#[tokio::test]
async fn live_replication_with_an_invalid_selector_ends_with_an_error() {
    for backend in backends("source") {
        let target = Backend::open(backend.name, "target");
        let (mut rx, _handle) = backend.db.replicate_to_live(
            &target.db,
            ReplicationOptions {
                retry: true,
                live: true,
                ..bad_selector()
            },
        );
        let mut events = Vec::new();
        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
        })
        .await;
        assert!(
            drained.is_ok(),
            "{}: live replication kept running",
            backend.name
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReplicationEvent::Error(m) if m.contains("$typo"))),
            "{}: {events:?}",
            backend.name
        );
        assert!(
            !events.iter().any(|e| matches!(e, ReplicationEvent::Paused)),
            "{}: {events:?}",
            backend.name
        );
    }
}
