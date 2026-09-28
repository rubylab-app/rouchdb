//! Conflict handling: local vs CouchDB divergent edits, three-way, resolution.

mod common;

use common::fresh_remote_db;
use rouchdb::{Database, GetOptions};

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn conflict_both_sides_modify_same_doc() {
    let url = fresh_remote_db("conflict_both").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": "original"}))
        .await
        .unwrap();
    let original_rev = r1.rev.unwrap();
    local.replicate_to(&remote).await.unwrap();

    let remote_doc = remote.get("doc1").await.unwrap();
    assert_eq!(remote_doc.rev.unwrap().to_string(), original_rev);

    assert!(
        local
            .update(
                "doc1",
                &original_rev,
                serde_json::json!({"v": "local_edit"}),
            )
            .await
            .unwrap()
            .ok
    );
    assert!(
        remote
            .update(
                "doc1",
                &original_rev,
                serde_json::json!({"v": "remote_edit"}),
            )
            .await
            .unwrap()
            .ok
    );

    let (push, pull) = local.sync(&remote).await.unwrap();
    assert!(push.ok);
    assert!(pull.ok);

    let local_doc = local
        .get_with_opts(
            "doc1",
            GetOptions {
                conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let remote_doc = remote
        .get_with_opts(
            "doc1",
            GetOptions {
                conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Winning rev must be the same (deterministic algorithm)
    assert_eq!(
        local_doc.rev.as_ref().unwrap().to_string(),
        remote_doc.rev.as_ref().unwrap().to_string(),
        "Winning revision must be the same on both sides"
    );

    assert!(
        local_doc.data.get("_conflicts").is_some(),
        "Local should have _conflicts"
    );
    assert!(
        remote_doc.data.get("_conflicts").is_some(),
        "Remote should have _conflicts"
    );
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn conflict_local_delete_remote_update() {
    let url = fresh_remote_db("conflict_delupd").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    let rev = r1.rev.unwrap();
    local.replicate_to(&remote).await.unwrap();

    assert!(local.remove("doc1", &rev).await.unwrap().ok);
    assert!(
        remote
            .update("doc1", &rev, serde_json::json!({"v": 2}))
            .await
            .unwrap()
            .ok
    );

    local.sync(&remote).await.unwrap();

    // The non-deleted version should win
    let remote_doc = remote.get("doc1").await.unwrap();
    assert_eq!(remote_doc.data["v"], 2, "Non-deleted revision should win");

    let local_doc = local.get("doc1").await.unwrap();
    assert_eq!(
        local_doc.data["v"], 2,
        "Local should agree with remote winner"
    );
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn conflict_remote_delete_local_update() {
    let url = fresh_remote_db("conflict_updel").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    let rev = r1.rev.unwrap();
    local.replicate_to(&remote).await.unwrap();

    assert!(remote.remove("doc1", &rev).await.unwrap().ok);
    assert!(
        local
            .update("doc1", &rev, serde_json::json!({"v": 2}))
            .await
            .unwrap()
            .ok
    );

    local.sync(&remote).await.unwrap();

    let local_doc = local.get("doc1").await.unwrap();
    assert_eq!(local_doc.data["v"], 2);

    let remote_doc = remote.get("doc1").await.unwrap();
    assert_eq!(remote_doc.data["v"], 2);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn conflict_three_way() {
    let url = fresh_remote_db("conflict_3way").await;
    let local1 = Database::memory("local1");
    let local2 = Database::memory("local2");
    let remote = Database::http(&url);

    let r1 = local1
        .put("doc1", serde_json::json!({"v": "original"}))
        .await
        .unwrap();
    let rev = r1.rev.unwrap();
    local1.replicate_to(&remote).await.unwrap();
    local2.replicate_from(&remote).await.unwrap();

    assert!(
        local1
            .update("doc1", &rev, serde_json::json!({"v": "local1_edit"}))
            .await
            .unwrap()
            .ok
    );
    assert!(
        local2
            .update("doc1", &rev, serde_json::json!({"v": "local2_edit"}))
            .await
            .unwrap()
            .ok
    );
    assert!(
        remote
            .update("doc1", &rev, serde_json::json!({"v": "remote_edit"}))
            .await
            .unwrap()
            .ok
    );

    local1.sync(&remote).await.unwrap();
    local2.sync(&remote).await.unwrap();
    local1.sync(&remote).await.unwrap();

    let d1 = local1.get("doc1").await.unwrap();
    let d2 = local2.get("doc1").await.unwrap();
    let dr = remote.get("doc1").await.unwrap();

    let rev1 = d1.rev.as_ref().unwrap().to_string();
    let rev2 = d2.rev.as_ref().unwrap().to_string();
    let revr = dr.rev.as_ref().unwrap().to_string();

    assert_eq!(rev1, rev2, "local1 and local2 must agree on winner");
    assert_eq!(rev2, revr, "local2 and remote must agree on winner");

    assert_eq!(d1.data["v"], d2.data["v"]);
    assert_eq!(d2.data["v"], dr.data["v"]);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn conflict_resolve_by_update() {
    let url = fresh_remote_db("conflict_resolve").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    let rev = r1.rev.unwrap();
    local.replicate_to(&remote).await.unwrap();

    assert!(
        local
            .update("doc1", &rev, serde_json::json!({"v": "local"}))
            .await
            .unwrap()
            .ok
    );
    assert!(
        remote
            .update("doc1", &rev, serde_json::json!({"v": "remote"}))
            .await
            .unwrap()
            .ok
    );

    local.sync(&remote).await.unwrap();

    let doc = local
        .get_with_opts(
            "doc1",
            GetOptions {
                conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let conflicts = doc
        .data
        .get("_conflicts")
        .and_then(|c| c.as_array())
        .expect("Should have conflicts");
    assert!(!conflicts.is_empty());

    let winner_rev = doc.rev.unwrap().to_string();
    assert!(
        local
            .update("doc1", &winner_rev, serde_json::json!({"v": "resolved"}))
            .await
            .unwrap()
            .ok
    );

    local.sync(&remote).await.unwrap();

    let local_doc = local.get("doc1").await.unwrap();
    let remote_doc = remote.get("doc1").await.unwrap();
    assert_eq!(local_doc.data["v"], "resolved");
    assert_eq!(remote_doc.data["v"], "resolved");
}
