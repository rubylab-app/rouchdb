/// Storage-independent write planning shared by the local adapters.
///
/// Every local adapter must apply exactly the same rules when a document is
/// written, otherwise the same call succeeds on one backend and silently
/// loses data on another. The functions here take the document's current
/// revision tree (and the parent revision's attachments) and decide what to
/// store; the adapter only loads the inputs and persists the resulting
/// [`PlannedWrite`].
///
/// The rules follow CouchDB / PouchDB (`updateDoc` in pouchdb-adapter-utils):
///
/// - an edit must extend a leaf (not necessarily the winner), so a losing
///   conflict branch can be updated or deleted;
/// - re-creating a deleted document extends its tombstone;
/// - tombstones carry no attachments; an explicit attachment set is
///   authoritative (stubs keep a parent attachment, omitted ones are
///   dropped), while an edit with no attachments inherits the parent's;
/// - a stub must reference an attachment the parent revision has;
/// - a replicated revision that is already stored is a no-op.
use std::collections::HashMap;

use crate::document::{AttachmentMeta, DocResult, Document, Revision, generate_rev_hash};
use crate::merge::{MergeResult, is_deleted, merge_tree, winning_rev};
use crate::rev_tree::{
    NodeOpts, RevNode, RevPath, RevStatus, RevTree, build_path_from_revs, path_from_revisions,
    rev_exists,
};

/// Everything an adapter needs to persist one document write.
#[derive(Debug, Clone)]
pub struct PlannedWrite {
    pub id: String,
    /// The revision being stored.
    pub rev: Revision,
    /// The merged (and stemmed) revision tree.
    pub tree: RevTree,
    /// Whether the stored revision is a deletion.
    pub deleted: bool,
    /// Whether the document's winning revision is deleted after the write.
    /// This is what the changes feed must report.
    pub doc_deleted: bool,
    /// The body to store for `rev`.
    pub data: serde_json::Value,
    /// The final attachment set of `rev` (metadata only, no inline bytes).
    pub attachments: HashMap<String, AttachmentMeta>,
    /// Inline attachment bytes to store, keyed by digest.
    pub new_blobs: Vec<(String, Vec<u8>)>,
    /// Digests of stubs that the attachment store must already hold
    /// (replicated writes); the adapter reports `missing_stub` otherwise.
    pub required_blobs: Vec<String>,
}

/// Outcome of planning a replicated (`new_edits=false`) write.
#[derive(Debug, Clone)]
pub enum ReplicatedWrite {
    /// The revision must be stored.
    Write(Box<PlannedWrite>),
    /// The revision is already stored with its body: nothing to do.
    AlreadyStored(DocResult),
}

/// Build a failed `DocResult`.
pub fn error_result(id: &str, error: &str, reason: &str) -> DocResult {
    DocResult {
        ok: false,
        id: id.to_string(),
        rev: None,
        error: Some(error.into()),
        reason: Some(reason.into()),
    }
}

/// Build a successful `DocResult`.
pub fn ok_result(id: &str, rev: &Revision) -> DocResult {
    DocResult {
        ok: true,
        id: id.to_string(),
        rev: Some(rev.to_string()),
        error: None,
        reason: None,
    }
}

fn conflict(id: &str) -> DocResult {
    error_result(id, "conflict", "Document update conflict")
}

/// The revision a `new_edits=true` write builds on: the supplied `_rev`, or
/// the deleted winner when a deleted document is re-created without one.
///
/// Adapters use this to load the parent's attachments before calling
/// [`plan_new_edit`].
pub fn edit_parent(existing: Option<&RevTree>, doc: &Document) -> Option<Revision> {
    if doc.rev.is_some() {
        return doc.rev.clone();
    }
    let tree = existing?;
    if !doc.deleted && is_deleted(tree) {
        return winning_rev(tree);
    }
    None
}

/// Plan a `new_edits=true` write of `doc` (which must already have gone
/// through [`Document::prepare_for_write`] and have a non-empty id).
///
/// `parent_attachments` are the attachments stored for
/// [`edit_parent`]`(existing, &doc)`. When `inherit_attachments` is set and
/// the document carries no attachments, the parent's attachments are kept
/// (a body-only edit); otherwise `doc.attachments` is the exact new set.
///
/// Returns the failed `DocResult` on conflict or invalid input.
pub fn plan_new_edit(
    existing: Option<&RevTree>,
    doc: Document,
    parent_attachments: Option<&HashMap<String, AttachmentMeta>>,
    inherit_attachments: bool,
    rev_limit: u64,
) -> std::result::Result<PlannedWrite, DocResult> {
    let id = doc.id.clone();
    let parent = edit_parent(existing, &doc);

    match (existing, &doc.rev) {
        (None, Some(_)) => return Err(error_result(&id, "not_found", "missing")),
        (Some(tree), None) if parent.is_none() && !tree.is_empty() => return Err(conflict(&id)),
        _ => {}
    }

    // Resolve the final attachment set before hashing, so the revision id
    // covers the attachments.
    let empty = HashMap::new();
    let parent_atts = parent_attachments.unwrap_or(&empty);
    let mut attachments = HashMap::new();
    let mut new_blobs = Vec::new();
    if doc.deleted {
        // Tombstones carry no attachments (CouchDB drops them on delete).
    } else if doc.attachments.is_empty() && inherit_attachments {
        attachments = parent_atts.clone();
    } else {
        for (name, mut meta) in doc.attachments {
            if let Some(bytes) = meta.data.take() {
                meta.digest = crate::document::attachment_digest(&bytes);
                meta.length = bytes.len() as u64;
                meta.stub = true;
                new_blobs.push((meta.digest.clone(), bytes));
                attachments.insert(name, meta);
            } else {
                // A stub must reference an attachment of the parent revision
                // (by name, or by digest when it was renamed).
                let found = parent_atts
                    .get(&name)
                    .filter(|p| meta.digest.is_empty() || p.digest == meta.digest)
                    .or_else(|| parent_atts.values().find(|p| p.digest == meta.digest));
                match found {
                    Some(p) => {
                        attachments.insert(name, p.clone());
                    }
                    None => {
                        return Err(error_result(
                            &id,
                            "missing_stub",
                            &format!("Invalid attachment stub in {} for {}", id, name),
                        ));
                    }
                }
            }
        }
    }

    let parent_str = parent.as_ref().map(|r| r.to_string());
    let new_pos = parent.as_ref().map(|r| r.pos + 1).unwrap_or(1);
    let new_hash = generate_rev_hash(&doc.data, doc.deleted, parent_str.as_deref(), &attachments);
    let rev = Revision::new(new_pos, new_hash.clone());

    let mut hashes = vec![new_hash];
    if let Some(ref p) = parent {
        hashes.push(p.hash.clone());
    }
    let path = build_path_from_revs(
        new_pos,
        &hashes,
        NodeOpts {
            deleted: doc.deleted,
        },
        RevStatus::Available,
    );

    let empty_tree = Vec::new();
    let tree = existing.unwrap_or(&empty_tree);
    let previously_deleted = !tree.is_empty() && is_deleted(tree);
    let (merged, result) = merge_tree(tree, &path, rev_limit);

    // Same conflict rule as PouchDB's updateDoc: the edit must add a new leaf
    // under an existing leaf (any leaf, not only the winner). Re-creating a
    // deleted document only fails if it would open a new branch.
    let in_conflict = !tree.is_empty()
        && match (previously_deleted, doc.deleted) {
            (true, false) => result == MergeResult::NewBranch,
            _ => result != MergeResult::NewLeaf,
        };
    if in_conflict {
        return Err(conflict(&id));
    }

    let doc_deleted = is_deleted(&merged);
    Ok(PlannedWrite {
        id,
        rev,
        tree: merged,
        deleted: doc.deleted,
        doc_deleted,
        data: doc.data,
        attachments,
        new_blobs,
        required_blobs: Vec::new(),
    })
}

/// Plan a replicated (`new_edits=false`) write: the revision id and its
/// ancestry (`_revisions` in the body) are taken as-is and merged into the
/// tree.
///
/// `has_body` tells whether the adapter already stores a body for
/// `doc.rev`; if so (and the revision is in the tree) the write is a no-op,
/// so a retried replication batch neither bumps the sequence nor overwrites
/// the stored body and attachments.
pub fn plan_replicated_edit(
    existing: Option<&RevTree>,
    mut doc: Document,
    has_body: bool,
    rev_limit: u64,
) -> std::result::Result<ReplicatedWrite, DocResult> {
    let id = doc.id.clone();
    let rev = match &doc.rev {
        Some(r) => r.clone(),
        None => return Err(error_result(&id, "bad_request", "missing _rev")),
    };
    if id.is_empty() {
        return Err(error_result(&id, "bad_request", "missing _id"));
    }

    let opts = NodeOpts {
        deleted: doc.deleted,
    };
    let path = match doc.data.get("_revisions") {
        Some(revisions) => {
            let start = revisions
                .get("start")
                .and_then(|v| v.as_u64())
                .unwrap_or(rev.pos);
            let ids: Vec<String> = revisions
                .get("ids")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            path_from_revisions(&rev, start, &ids, opts, RevStatus::Available)
                .map_err(|e| error_result(&id, "bad_request", &e.to_string()))?
        }
        // No ancestry available: a single-node path.
        None => RevPath {
            pos: rev.pos,
            tree: RevNode {
                hash: rev.hash.clone(),
                status: RevStatus::Available,
                opts,
                children: vec![],
            },
        },
    };
    doc.strip_metadata_members();

    if has_body && existing.is_some_and(|t| rev_exists(t, rev.pos, &rev.hash)) {
        return Ok(ReplicatedWrite::AlreadyStored(ok_result(&id, &rev)));
    }

    let mut attachments = HashMap::new();
    let mut new_blobs = Vec::new();
    let mut required_blobs = Vec::new();
    for (name, mut meta) in doc.attachments {
        if let Some(bytes) = meta.data.take() {
            meta.digest = crate::document::attachment_digest(&bytes);
            meta.length = bytes.len() as u64;
            new_blobs.push((meta.digest.clone(), bytes));
        } else {
            required_blobs.push(meta.digest.clone());
        }
        meta.stub = true;
        attachments.insert(name, meta);
    }

    let empty_tree = Vec::new();
    let (merged, _) = merge_tree(existing.unwrap_or(&empty_tree), &path, rev_limit);
    let doc_deleted = is_deleted(&merged);

    Ok(ReplicatedWrite::Write(Box::new(PlannedWrite {
        id,
        rev,
        tree: merged,
        deleted: doc.deleted,
        doc_deleted,
        data: doc.data,
        attachments,
        new_blobs,
        required_blobs,
    })))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::collect_conflicts;

    fn doc(id: &str, rev: Option<&str>, data: serde_json::Value) -> Document {
        Document {
            id: id.into(),
            rev: rev.map(|r| r.parse().unwrap()),
            deleted: false,
            data,
            attachments: HashMap::new(),
        }
    }

    fn inline(bytes: &[u8]) -> AttachmentMeta {
        AttachmentMeta {
            content_type: "text/plain".into(),
            digest: String::new(),
            length: 0,
            stub: false,
            data: Some(bytes.to_vec()),
        }
    }

    fn write(tree: Option<&RevTree>, d: Document) -> PlannedWrite {
        plan_new_edit(tree, d, None, true, 1000).unwrap()
    }

    /// Build 1-x with two conflicting children and return (tree, winner, loser).
    fn conflicted() -> (RevTree, String, String) {
        let w1 = write(None, doc("d", None, serde_json::json!({"v": 1})));
        let a = write(
            Some(&w1.tree),
            doc(
                "d",
                Some(&w1.rev.to_string()),
                serde_json::json!({"v": "a"}),
            ),
        );
        let b = plan_replicated_edit(
            Some(&a.tree),
            doc(
                "d",
                Some(&format!("2-{}", "0".repeat(32))),
                serde_json::json!({"_revisions": {"start": 2, "ids": ["0".repeat(32), w1.rev.hash.clone()]}}),
            ),
            false,
            1000,
        )
        .unwrap();
        let ReplicatedWrite::Write(b) = b else {
            panic!()
        };
        let winner = winning_rev(&b.tree).unwrap().to_string();
        let loser = collect_conflicts(&b.tree)[0].to_string();
        (b.tree, winner, loser)
    }

    fn revs(v: &[Revision]) -> Vec<String> {
        v.iter().map(|r| r.to_string()).collect()
    }

    fn json<T: serde::Serialize>(v: &T) -> serde_json::Value {
        serde_json::to_value(v).unwrap()
    }

    #[test]
    fn can_update_and_delete_a_losing_leaf() {
        let (tree, winner, loser) = conflicted();
        let loser_rev: Revision = loser.parse().unwrap();
        // Updating the losing leaf extends it (and, one generation ahead, it
        // becomes the winner).
        let upd = plan_new_edit(
            Some(&tree),
            doc("d", Some(&loser), serde_json::json!({"v": "fixed"})),
            None,
            true,
            1000,
        )
        .unwrap();
        assert_eq!(upd.rev.pos, 3);
        assert_eq!(
            upd.rev.hash,
            generate_rev_hash(
                &serde_json::json!({"v": "fixed"}),
                false,
                Some(&loser),
                &HashMap::new()
            )
        );
        assert_eq!(
            crate::rev_tree::find_rev_ancestry(&upd.tree, 3, &upd.rev.hash).unwrap()[1],
            loser_rev.hash
        );
        assert_eq!(winning_rev(&upd.tree).unwrap(), upd.rev);
        assert_eq!(revs(&collect_conflicts(&upd.tree)), vec![winner.clone()]);
        // Deleting it resolves the conflict and leaves the winner in place.
        let mut del = doc("d", Some(&loser), serde_json::json!({}));
        del.deleted = true;
        let del = plan_new_edit(Some(&tree), del, None, true, 1000).unwrap();
        assert_eq!(del.rev.pos, 3);
        assert!(del.deleted);
        assert!(collect_conflicts(&del.tree).is_empty());
        assert_eq!(winning_rev(&del.tree).unwrap().to_string(), winner);
        assert!(!del.doc_deleted);
    }

    #[test]
    fn deletion_and_empty_edit_get_distinct_revs() {
        // A tombstone and an empty-body edit of the same parent are different
        // revisions (the hash covers `_deleted`), so replicating one into a
        // database holding the other surfaces two leaves instead of silently
        // treating them as the same revision.
        let w1 = write(None, doc("d", None, serde_json::json!({"v": 1})));
        let r1 = w1.rev.to_string();
        let edit = write(Some(&w1.tree), doc("d", Some(&r1), serde_json::json!({})));
        let mut del = doc("d", Some(&r1), serde_json::json!({}));
        del.deleted = true;
        let tomb = write(Some(&w1.tree), del);
        assert_eq!((edit.rev.pos, tomb.rev.pos), (2, 2));
        assert_ne!(edit.rev, tomb.rev);
        assert!(!edit.deleted && !edit.doc_deleted);
        assert!(tomb.deleted && tomb.doc_deleted);

        let replicated = doc(
            "d",
            Some(&tomb.rev.to_string()),
            serde_json::json!({"_revisions": {"start": 2, "ids": [tomb.rev.hash, w1.rev.hash]}}),
        );
        let mut replicated = replicated;
        replicated.deleted = true;
        let ReplicatedWrite::Write(both) =
            plan_replicated_edit(Some(&edit.tree), replicated, false, 1000).unwrap()
        else {
            panic!("a new tombstone must be written");
        };
        let mut leaves: Vec<String> = crate::rev_tree::collect_leaves(&both.tree)
            .iter()
            .map(|l| l.rev_string())
            .collect();
        leaves.sort();
        let mut expected = vec![edit.rev.to_string(), tomb.rev.to_string()];
        expected.sort();
        assert_eq!(leaves, expected);
        // The live edit wins over the tombstone.
        assert_eq!(winning_rev(&both.tree).unwrap(), edit.rev);
        assert!(!both.doc_deleted);
    }

    #[test]
    fn retrying_an_update_with_the_old_rev_conflicts() {
        // A retried PUT (same parent, same body) produces the revision that
        // already exists; its parent is no longer a leaf, so CouchDB answers
        // 409 rather than reporting a second successful write.
        let w1 = write(None, doc("d", None, serde_json::json!({"v": 1})));
        let r1 = w1.rev.to_string();
        let w2 = write(
            Some(&w1.tree),
            doc("d", Some(&r1), serde_json::json!({"v": 2})),
        );
        let err = plan_new_edit(
            Some(&w2.tree),
            doc("d", Some(&r1), serde_json::json!({"v": 2})),
            None,
            true,
            1000,
        )
        .unwrap_err();
        assert_eq!(err.error.as_deref(), Some("conflict"));
        assert_eq!(err.id, "d");
    }

    #[test]
    fn new_document_rev_is_generation_one_hash_of_body() {
        let data = serde_json::json!({"name": "Alice"});
        let expected = generate_rev_hash(&data, false, None, &HashMap::new());
        // No stored tree, or an empty one (nothing left after a purge), both
        // mean "new document".
        for existing in [None, Some(Vec::new())] {
            let w = plan_new_edit(
                existing.as_ref(),
                doc("d", None, data.clone()),
                None,
                true,
                1000,
            )
            .unwrap();
            assert_eq!(w.rev, Revision::new(1, expected.clone()));
            assert_eq!(w.data, data);
            assert!(!w.deleted && !w.doc_deleted);
            assert_eq!(winning_rev(&w.tree).unwrap(), w.rev);
        }
    }

    #[test]
    fn writes_apply_the_rev_limit() {
        // new_edits=true: the planned tree is stemmed to rev_limit.
        let mut w = write(None, doc("d", None, serde_json::json!({"v": 0})));
        for i in 1..5 {
            w = plan_new_edit(
                Some(&w.tree),
                doc("d", Some(&w.rev.to_string()), serde_json::json!({ "v": i })),
                None,
                true,
                3,
            )
            .unwrap();
        }
        assert_eq!(w.rev.pos, 5);
        assert_eq!(w.tree.len(), 1);
        assert_eq!(w.tree[0].pos, 3);
        assert_eq!(
            crate::rev_tree::find_rev_ancestry(&w.tree, 5, &w.rev.hash)
                .unwrap()
                .len(),
            3
        );

        // new_edits=false: a long `_revisions` list is stemmed too.
        let ids: Vec<String> = (1..=6).rev().map(|i| format!("{:032x}", i)).collect();
        let d = doc(
            "r",
            Some(&format!("6-{}", ids[0])),
            serde_json::json!({"_revisions": {"start": 6, "ids": ids}}),
        );
        let ReplicatedWrite::Write(p) = plan_replicated_edit(None, d, false, 2).unwrap() else {
            panic!("a new revision must be written");
        };
        assert_eq!(p.tree.len(), 1);
        assert_eq!(p.tree[0].pos, 5);
        assert_eq!(
            crate::rev_tree::find_rev_ancestry(&p.tree, 6, &ids[0]).unwrap(),
            [ids[0].clone(), ids[1].clone()]
        );
    }

    #[test]
    fn non_leaf_or_unknown_parent_conflicts() {
        let w1 = write(None, doc("d", None, serde_json::json!({"v": 1})));
        let r1 = w1.rev.to_string();
        let w2 = write(
            Some(&w1.tree),
            doc("d", Some(&r1), serde_json::json!({"v": 2})),
        );
        // 1-x is no longer a leaf.
        let err = plan_new_edit(
            Some(&w2.tree),
            doc("d", Some(&r1), serde_json::json!({"v": 3})),
            None,
            true,
            1000,
        )
        .unwrap_err();
        assert_eq!(err.error.as_deref(), Some("conflict"));
        // Unknown rev.
        let err = plan_new_edit(
            Some(&w2.tree),
            doc("d", Some("9-deadbeef"), serde_json::json!({})),
            None,
            true,
            1000,
        )
        .unwrap_err();
        assert_eq!(err.error.as_deref(), Some("conflict"));
        // Creating over a live doc.
        let err = plan_new_edit(
            Some(&w2.tree),
            doc("d", None, serde_json::json!({})),
            None,
            true,
            1000,
        )
        .unwrap_err();
        assert_eq!(err.error.as_deref(), Some("conflict"));
    }

    #[test]
    fn recreate_after_delete_extends_tombstone() {
        let w1 = write(None, doc("d", None, serde_json::json!({"v": 1})));
        let mut del = doc("d", Some(&w1.rev.to_string()), serde_json::json!({}));
        del.deleted = true;
        let w2 = write(Some(&w1.tree), del);
        assert!(w2.doc_deleted);
        // Same body as the original 1-x: must not collide with it.
        let w3 = write(Some(&w2.tree), doc("d", None, serde_json::json!({"v": 1})));
        assert_eq!(w3.rev.pos, 3);
        assert!(!w3.doc_deleted);
        assert_eq!(winning_rev(&w3.tree).unwrap(), w3.rev);
    }

    #[test]
    fn tombstones_drop_attachments_and_explicit_sets_are_exact() {
        let mut d = doc("d", None, serde_json::json!({}));
        d.attachments.insert("a".into(), inline(b"AAA"));
        d.attachments.insert("b".into(), inline(b"BBB"));
        let w1 = write(None, d);
        let mut blobs: Vec<(String, Vec<u8>)> = w1.new_blobs.clone();
        blobs.sort();
        let mut expected = vec![
            (crate::document::attachment_digest(b"AAA"), b"AAA".to_vec()),
            (crate::document::attachment_digest(b"BBB"), b"BBB".to_vec()),
        ];
        expected.sort();
        assert_eq!(blobs, expected);
        let parent = w1.attachments.clone();
        assert_eq!(
            parent["a"].digest,
            crate::document::attachment_digest(b"AAA")
        );
        assert_eq!((parent["a"].length, parent["a"].stub), (3, true));
        assert!(parent["a"].data.is_none());

        // Body-only edit inherits.
        let w2 = plan_new_edit(
            Some(&w1.tree),
            doc("d", Some(&w1.rev.to_string()), serde_json::json!({"v": 2})),
            Some(&parent),
            true,
            1000,
        )
        .unwrap();
        assert_eq!(json(&w2.attachments), json(&parent));
        assert!(w2.new_blobs.is_empty());

        // Explicit set with one stub: the omitted attachment is dropped.
        let mut d3 = doc("d", Some(&w2.rev.to_string()), serde_json::json!({}));
        let mut stub = parent["a"].clone();
        stub.stub = true;
        d3.attachments.insert("a".into(), stub);
        let w3 = plan_new_edit(Some(&w2.tree), d3, Some(&parent), true, 1000).unwrap();
        assert_eq!(w3.attachments.keys().collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(json(&w3.attachments["a"]), json(&parent["a"]));

        // Tombstone.
        let mut del = doc("d", Some(&w2.rev.to_string()), serde_json::json!({}));
        del.deleted = true;
        let w4 = plan_new_edit(Some(&w2.tree), del, Some(&parent), true, 1000).unwrap();
        assert!(w4.attachments.is_empty());
    }

    #[test]
    fn unknown_stub_is_missing_stub() {
        let w1 = write(None, doc("d", None, serde_json::json!({})));
        let mut d = doc("d", Some(&w1.rev.to_string()), serde_json::json!({}));
        d.attachments.insert(
            "x".into(),
            AttachmentMeta {
                content_type: "text/plain".into(),
                digest: "md5-bogus".into(),
                length: 3,
                stub: true,
                data: None,
            },
        );
        let err = plan_new_edit(Some(&w1.tree), d, Some(&HashMap::new()), true, 1000).unwrap_err();
        assert_eq!(err.error.as_deref(), Some("missing_stub"));
    }

    #[test]
    fn attachment_only_edits_get_distinct_revs() {
        let w1 = write(None, doc("d", None, serde_json::json!({})));
        let r1 = w1.rev.to_string();
        let mut a = doc("d", Some(&r1), serde_json::json!({}));
        a.attachments.insert("f".into(), inline(b"AAA"));
        let mut b = doc("d", Some(&r1), serde_json::json!({}));
        b.attachments.insert("f".into(), inline(b"BBB"));
        let wa = write(Some(&w1.tree), a);
        let wb = write(Some(&w1.tree), b);
        assert_ne!(wa.rev, wb.rev);
    }

    #[test]
    fn replicated_existing_rev_is_noop() {
        let w1 = write(None, doc("d", None, serde_json::json!({"v": 1})));
        let again = doc("d", Some(&w1.rev.to_string()), serde_json::json!({"v": 99}));
        match plan_replicated_edit(Some(&w1.tree), again.clone(), true, 1000).unwrap() {
            ReplicatedWrite::AlreadyStored(r) => {
                assert_eq!(json(&r), json(&ok_result("d", &w1.rev)));
            }
            ReplicatedWrite::Write(_) => panic!("existing rev must not be rewritten"),
        }
        // Without a stored body it is written (and the node becomes available).
        assert!(matches!(
            plan_replicated_edit(Some(&w1.tree), again, false, 1000).unwrap(),
            ReplicatedWrite::Write(_)
        ));
    }

    #[test]
    fn replicated_invalid_revisions_rejected() {
        let d = doc(
            "d",
            Some("3-c"),
            serde_json::json!({"_revisions": {"start": 3, "ids": []}}),
        );
        let err = plan_replicated_edit(None, d, false, 1000).unwrap_err();
        assert_eq!(err.error.as_deref(), Some("bad_request"));
        let d = doc(
            "d",
            Some("3-c"),
            serde_json::json!({"_revisions": {"start": 2, "ids": ["c", "b"]}}),
        );
        let err = plan_replicated_edit(None, d, false, 1000).unwrap_err();
        assert_eq!(err.error.as_deref(), Some("bad_request"));
        // A replicated write needs a revision and an id.
        let err = plan_replicated_edit(None, doc("d", None, serde_json::json!({})), false, 1000)
            .unwrap_err();
        assert_eq!(err.error.as_deref(), Some("bad_request"));
        let err = plan_replicated_edit(
            None,
            doc("", Some("1-a"), serde_json::json!({})),
            false,
            1000,
        )
        .unwrap_err();
        assert_eq!(err.error.as_deref(), Some("bad_request"));
    }

    #[test]
    fn replicated_deletion_of_winner_reports_live_doc() {
        // Two live leaves; replicating a tombstone on the winner leaves the
        // loser as the new (live) winner, so the doc is not deleted.
        let (tree, winner, _loser) = conflicted();
        let w: Revision = winner.parse().unwrap();
        let tomb_hash = "f".repeat(32);
        let mut d = doc(
            "d",
            Some(&format!("{}-{}", w.pos + 1, tomb_hash)),
            serde_json::json!({"_revisions": {"start": w.pos + 1, "ids": [tomb_hash, w.hash]}}),
        );
        d.deleted = true;
        let ReplicatedWrite::Write(p) = plan_replicated_edit(Some(&tree), d, false, 1000).unwrap()
        else {
            panic!()
        };
        assert!(p.deleted);
        assert!(!p.doc_deleted);
    }
}
