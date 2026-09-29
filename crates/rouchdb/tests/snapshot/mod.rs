//! The full replicated state of a database, for comparing both ends of a
//! replication: every document (tombstones included) with its leaves,
//! winning revision and history, conflicts, body and attachment bytes.
#![allow(dead_code)]

use std::collections::BTreeMap;

use rouchdb::{ChangesOptions, ChangesStyle, Database, GetOptions};

#[derive(Debug, Clone, PartialEq)]
pub struct DocState {
    /// Winning revision.
    pub rev: String,
    pub deleted: bool,
    /// Every leaf revision (deleted ones included), sorted.
    pub leaves: Vec<String>,
    /// `_conflicts` of the winner, sorted (always empty for tombstones).
    pub conflicts: Vec<String>,
    /// Revision ids of the winner's history, newest first (`_revisions`).
    pub history: Vec<String>,
    /// The body without `_` members.
    pub body: serde_json::Value,
    /// Attachment name -> (content type, bytes).
    pub attachments: BTreeMap<String, (String, Vec<u8>)>,
}

pub type Snapshot = BTreeMap<String, DocState>;

fn rev_key(rev: &str) -> (u64, String) {
    let (generation, hash) = rev.split_once('-').expect("rev is N-hash");
    (generation.parse().expect("numeric generation"), hash.into())
}

/// Read the whole state of `db` through its public API.
pub async fn snapshot(db: &Database) -> Snapshot {
    let feed = db
        .changes(ChangesOptions {
            style: ChangesStyle::AllDocs,
            ..Default::default()
        })
        .await
        .unwrap();
    let mut out = Snapshot::new();
    for change in feed.results {
        let mut leaves: Vec<String> = change.changes.iter().map(|c| c.rev.clone()).collect();
        leaves.sort();
        let doc = if change.deleted {
            // The winner of an all-deleted document is its highest leaf.
            let winner = leaves.iter().max_by_key(|r| rev_key(r)).unwrap().clone();
            db.get_with_opts(
                &change.id,
                GetOptions {
                    rev: Some(winner),
                    revs: true,
                    ..Default::default()
                },
            )
            .await
        } else {
            db.get_with_opts(
                &change.id,
                GetOptions {
                    conflicts: true,
                    revs: true,
                    ..Default::default()
                },
            )
            .await
        }
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", change.id));

        let strings = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect())
                .unwrap_or_default()
        };
        let mut conflicts = strings(&doc.data["_conflicts"]);
        conflicts.sort();
        let history = strings(&doc.data["_revisions"]["ids"]);
        let body: serde_json::Map<String, serde_json::Value> = doc
            .data
            .as_object()
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| !k.starts_with('_'))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let mut attachments = BTreeMap::new();
        if !doc.deleted {
            for (name, meta) in &doc.attachments {
                let bytes = db.get_attachment(&change.id, name).await.unwrap();
                attachments.insert(name.clone(), (meta.content_type.clone(), bytes));
            }
        }
        out.insert(
            change.id.clone(),
            DocState {
                rev: doc.rev.expect("a read doc has a rev").to_string(),
                deleted: doc.deleted,
                leaves,
                conflicts,
                history,
                body: serde_json::Value::Object(body),
                attachments,
            },
        );
    }
    out
}

/// Assert that `a` and `b` hold exactly the same documents, revisions,
/// conflicts, tombstones and attachments; returns that state.
pub async fn assert_same_state(a: &Database, b: &Database) -> Snapshot {
    let left = snapshot(a).await;
    let right = snapshot(b).await;
    assert_eq!(left, right, "the two databases diverge");
    left
}
