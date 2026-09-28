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
/// - an edit naming a revision of a document that does not exist is a
///   conflict;
/// - tombstones carry no attachments; an explicit attachment set is
///   authoritative (stubs keep a parent attachment, omitted ones are
///   dropped), while an edit with no attachments inherits the parent's;
/// - a stub keeps the parent revision's attachment of the same name (the
///   stored metadata wins, as in CouchDB); a stub for a name the parent
///   does not have is `missing_stub`;
/// - a replicated revision that is already stored is a no-op, and its
///   `_revisions` history must be well formed;
/// - revisions stemmed by the revision limit no longer exist
///   ([`PlannedWrite::stemmed`]);
/// - `_local/` documents are not versioned documents ([`plan_local_write`]).
use std::collections::HashMap;

use crate::document::{
    AttachmentMeta, DocResult, Document, Revision, generate_rev_hash, normalize_rev_hash,
};
use crate::merge::{MergeResult, is_deleted, merge_and_stem, winning_rev};
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
    /// Revisions the revision limit removed from the tree: their stored
    /// bodies must be dropped (CouchDB reports them missing).
    pub stemmed: Vec<Revision>,
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
        // CouchDB (PUT and `_bulk_docs`) and PouchDB: a revision of a
        // document that does not exist is a conflict, not "missing".
        (None, Some(_)) => return Err(conflict(&id)),
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
                // Like CouchDB, a stub is matched by name only and keeps the
                // parent's stored attachment (its digest, type and length
                // win over the stub's).
                match parent_atts.get(&name) {
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
    let (merged, result, stemmed) = merge_and_stem(tree, &path, rev_limit);

    // The edit must add a new leaf under an existing leaf (any leaf, not
    // only the winner); re-creating a deleted document extends its
    // tombstone (see `edit_parent`). Unlike PouchDB, which lets a re-created
    // document land on a revision that already exists, re-sending an old
    // edit of a deleted document is a conflict, as in CouchDB.
    let in_conflict = !tree.is_empty() && result != MergeResult::NewLeaf;
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
        stemmed,
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
        Some(r) => r.clone().normalized(),
        None => return Err(error_result(&id, "bad_request", "missing _rev")),
    };
    if id.is_empty() {
        return Err(error_result(&id, "bad_request", "missing _id"));
    }
    if rev.hash.is_empty() || rev.hash.contains('\0') {
        return Err(error_result(&id, "bad_request", "Invalid rev format"));
    }
    if let Err(e) = crate::json::check_document_depth(&doc.data) {
        return Err(error_result(&id, "bad_request", &reason(e)));
    }

    let opts = NodeOpts {
        deleted: doc.deleted,
    };
    let path = match doc.data.get("_revisions") {
        Some(revisions) => {
            let (start, ids) = parse_revisions(revisions)
                .map_err(|why| error_result(&id, "doc_validation", why))?;
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
    let (merged, _, stemmed) = merge_and_stem(existing.unwrap_or(&empty_tree), &path, rev_limit);
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
        stemmed,
    })))
}

/// The reason of an error, without the `Display` prefix of its kind.
fn reason(e: crate::error::RouchError) -> String {
    match e {
        crate::error::RouchError::BadRequest(reason) => reason,
        other => other.to_string(),
    }
}

/// Parse a `_revisions` member (`{"start": N, "ids": [...]}`), rejecting
/// what CouchDB rejects with the same reasons, and normalizing the ids.
fn parse_revisions(revisions: &serde_json::Value) -> Result<(u64, Vec<String>), &'static str> {
    let obj = revisions
        .as_object()
        .ok_or("Bad special document member: _revisions")?;
    let start = obj
        .get("start")
        .and_then(|v| v.as_u64())
        .ok_or("_revisions.start isn't an integer.")?;
    let ids = obj
        .get("ids")
        .and_then(|v| v.as_array())
        .ok_or("_revisions.ids isn't a array.")?;
    let ids = ids
        .iter()
        .map(|v| {
            v.as_str()
                .map(|id| normalize_rev_hash(id).into_owned())
                .ok_or("RevId isn't a string")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if ids.iter().any(|id| id.contains('\0')) {
        return Err("Invalid rev format");
    }
    Ok((start, ids))
}

// ---------------------------------------------------------------------------
// Local documents
// ---------------------------------------------------------------------------

/// The id of a local document without its `_local/` prefix, if `id` names
/// one.
pub fn local_doc_id(id: &str) -> Option<&str> {
    id.strip_prefix("_local/")
}

/// A planned write of a `_local/` document.
#[derive(Debug, Clone)]
pub enum LocalWrite {
    /// Store `body` (which carries its `_rev`) under `id` (no `_local/`).
    Put {
        id: String,
        body: serde_json::Value,
        result: DocResult,
    },
    /// Remove the local document `id` (a missing one is not an error).
    Delete { id: String, result: DocResult },
}

/// Plan a `new_edits=true` write of a `_local/` document (which must have
/// gone through [`Document::prepare_for_write`]) the way CouchDB does:
/// local documents are not replicated, listed or versioned in a revision
/// tree; there is no conflict check, the new revision is `0-(N+1)` where
/// `0-N` is the revision sent (`0-1` without one), a deletion is `0-0`, and
/// attachments are not kept.
pub fn plan_local_write(doc: Document) -> std::result::Result<LocalWrite, DocResult> {
    let local_id = match local_doc_id(&doc.id) {
        Some(local) if !local.is_empty() => local.to_string(),
        _ => {
            return Err(error_result(
                &doc.id,
                "illegal_docid",
                &format!("Illegal document id `{}`", doc.id),
            ));
        }
    };
    let counter = match &doc.rev {
        None => 0,
        Some(rev) if rev.pos == 0 => match rev.hash.parse::<u64>() {
            Ok(n) => n,
            Err(_) => return Err(error_result(&doc.id, "bad_request", "Invalid rev format")),
        },
        Some(_) => return Err(error_result(&doc.id, "bad_request", "Invalid rev format")),
    };
    let result = |rev: String| DocResult {
        ok: true,
        id: doc.id.clone(),
        rev: Some(rev),
        error: None,
        reason: None,
    };
    if doc.deleted {
        return Ok(LocalWrite::Delete {
            result: result("0-0".into()),
            id: local_id,
        });
    }
    let rev = format!("0-{}", counter.saturating_add(1));
    let mut body = match doc.data {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    body.insert("_rev".into(), serde_json::Value::String(rev.clone()));
    Ok(LocalWrite::Put {
        result: result(rev),
        id: local_id,
        body: serde_json::Value::Object(body),
    })
}

/// A stored local document (`_rev` inside the body, as written by
/// [`plan_local_write`] or `put_local`) as a [`Document`] with id
/// `_local/{local_id}`.
pub fn local_document(local_id: &str, stored: serde_json::Value) -> Document {
    let mut body = match stored {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    body.remove("_id");
    let rev = body
        .remove("_rev")
        .and_then(|r| r.as_str().and_then(|r| r.parse().ok()))
        .unwrap_or_else(|| Revision::new(0, "1".into()));
    Document {
        id: format!("_local/{}", local_id),
        rev: Some(rev),
        deleted: false,
        data: serde_json::Value::Object(body),
        attachments: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::collect_conflicts;
    use crate::rev_tree::rev_exists;

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

    #[test]
    fn can_update_and_delete_a_losing_leaf() {
        let (tree, _winner, loser) = conflicted();
        // Updating the losing leaf extends it.
        let upd = plan_new_edit(
            Some(&tree),
            doc("d", Some(&loser), serde_json::json!({"v": "fixed"})),
            None,
            true,
            1000,
        );
        assert!(upd.is_ok());
        // Deleting it resolves the conflict.
        let mut del = doc("d", Some(&loser), serde_json::json!({}));
        del.deleted = true;
        let del = plan_new_edit(Some(&tree), del, None, true, 1000).unwrap();
        assert!(collect_conflicts(&del.tree).is_empty());
        assert!(!del.doc_deleted);
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
        assert_eq!(w1.new_blobs.len(), 2);
        let parent = w1.attachments.clone();

        // Body-only edit inherits.
        let w2 = plan_new_edit(
            Some(&w1.tree),
            doc("d", Some(&w1.rev.to_string()), serde_json::json!({"v": 2})),
            Some(&parent),
            true,
            1000,
        )
        .unwrap();
        assert_eq!(w2.attachments.len(), 2);

        // Explicit set with one stub: the omitted attachment is dropped.
        let mut d3 = doc("d", Some(&w2.rev.to_string()), serde_json::json!({}));
        let mut stub = parent["a"].clone();
        stub.stub = true;
        d3.attachments.insert("a".into(), stub);
        let w3 = plan_new_edit(Some(&w2.tree), d3, Some(&parent), true, 1000).unwrap();
        assert_eq!(w3.attachments.keys().collect::<Vec<_>>(), vec!["a"]);

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
            ReplicatedWrite::AlreadyStored(r) => assert!(r.ok),
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
        assert!(plan_replicated_edit(None, d, false, 1000).is_err());
    }

    #[test]
    fn revision_of_a_missing_document_conflicts() {
        for deleted in [false, true] {
            let mut d = doc("d", Some("1-abc"), serde_json::json!({}));
            d.deleted = deleted;
            let err = plan_new_edit(None, d, None, true, 1000).unwrap_err();
            assert_eq!(err.error.as_deref(), Some("conflict"));
        }
    }

    #[test]
    fn stubs_are_matched_by_name() {
        let mut d = doc("d", None, serde_json::json!({}));
        d.attachments.insert("a".into(), inline(b"AAA"));
        let w1 = write(None, d);
        let parent = w1.attachments.clone();
        let stub = |digest: &str| AttachmentMeta {
            content_type: "image/png".into(),
            digest: digest.into(),
            length: 99,
            stub: true,
            data: None,
        };
        // Same name, other (or no) digest: the parent's attachment is kept.
        for digest in ["md5-other", ""] {
            let mut d = doc("d", Some(&w1.rev.to_string()), serde_json::json!({}));
            d.attachments.insert("a".into(), stub(digest));
            let w = plan_new_edit(Some(&w1.tree), d, Some(&parent), true, 1000).unwrap();
            assert_eq!(w.attachments["a"].digest, parent["a"].digest);
            assert_eq!(w.attachments["a"].content_type, "text/plain");
            assert_eq!(w.attachments["a"].length, 3);
        }
        // A new name with the parent's digest is not a rename.
        let mut d = doc("d", Some(&w1.rev.to_string()), serde_json::json!({}));
        d.attachments.insert("b".into(), stub(&parent["a"].digest));
        let err = plan_new_edit(Some(&w1.tree), d, Some(&parent), true, 1000).unwrap_err();
        assert_eq!(err.error.as_deref(), Some("missing_stub"));
        assert_eq!(
            err.reason.as_deref(),
            Some("Invalid attachment stub in d for b")
        );
    }

    #[test]
    fn writes_report_stemmed_revisions() {
        let mut w = write(None, doc("d", None, serde_json::json!({"v": 0})));
        let first = w.rev.clone();
        for v in 1..3 {
            let d = doc("d", Some(&w.rev.to_string()), serde_json::json!({"v": v}));
            w = plan_new_edit(Some(&w.tree), d, None, true, 3).unwrap();
            assert!(w.stemmed.is_empty());
        }
        let d = doc("d", Some(&w.rev.to_string()), serde_json::json!({"v": 3}));
        let w = plan_new_edit(Some(&w.tree), d, None, true, 3).unwrap();
        assert_eq!(w.stemmed, [first]);
        // Replicated writes report them too.
        let hashes: Vec<String> = (0..5).map(|i| format!("{:032x}", 5 - i)).collect();
        let d = doc(
            "r",
            Some(&format!("5-{}", hashes[0])),
            serde_json::json!({"_revisions": {"start": 5, "ids": hashes}}),
        );
        let ReplicatedWrite::Write(p) = plan_replicated_edit(None, d, false, 2).unwrap() else {
            panic!()
        };
        let stemmed: Vec<String> = p.stemmed.iter().map(|r| r.to_string()).collect();
        assert_eq!(
            stemmed,
            (1..=3)
                .map(|pos| format!("{}-{:032x}", pos, pos))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn replicated_revisions_are_type_checked() {
        let cases = [
            (
                serde_json::json!({"start": 3, "ids": ["c", 5, "a"]}),
                "RevId isn't a string",
            ),
            (
                serde_json::json!({"start": 3.5, "ids": ["c"]}),
                "_revisions.start isn't an integer.",
            ),
            (
                serde_json::json!({"start": -3, "ids": ["c"]}),
                "_revisions.start isn't an integer.",
            ),
            (
                serde_json::json!({"start": 3, "ids": {"0": "c"}}),
                "_revisions.ids isn't a array.",
            ),
            (
                serde_json::json!("3-c"),
                "Bad special document member: _revisions",
            ),
            (
                serde_json::json!({"start": 3, "ids": ["c", "b\u{0}"]}),
                "Invalid rev format",
            ),
        ];
        for (revisions, reason) in cases {
            let d = doc(
                "d",
                Some("3-c"),
                serde_json::json!({"_revisions": revisions}),
            );
            let err = plan_replicated_edit(None, d, false, 1000).unwrap_err();
            assert_eq!(err.reason.as_deref(), Some(reason), "{revisions}");
        }
        // A revision id with NUL is refused even when built directly.
        let d = Document {
            rev: Some(Revision::new(1, "a\u{0}b".into())),
            ..doc("d", None, serde_json::json!({}))
        };
        assert!(plan_replicated_edit(None, d, false, 1000).is_err());
    }

    #[test]
    fn replicated_revisions_are_normalized() {
        let (up, base) = ("C".repeat(32), "B".repeat(32));
        let d = doc(
            "d",
            Some(&format!("2-{up}")),
            serde_json::json!({"_revisions": {"start": 2, "ids": [up, base]}}),
        );
        let ReplicatedWrite::Write(p) = plan_replicated_edit(None, d, false, 1000).unwrap() else {
            panic!()
        };
        assert_eq!(p.rev.hash, "c".repeat(32));
        assert!(rev_exists(&p.tree, 1, &"b".repeat(32)));
        assert!(rev_exists(&p.tree, 2, &"c".repeat(32)));
    }

    #[test]
    fn replicated_documents_are_depth_checked() {
        let mut deep = serde_json::json!(1);
        for _ in 0..crate::json::MAX_NESTING_DEPTH {
            deep = serde_json::json!([deep]);
        }
        let d = doc("d", Some("1-a"), serde_json::json!({ "v": deep }));
        let err = plan_replicated_edit(None, d, false, 1000).unwrap_err();
        assert_eq!(err.error.as_deref(), Some("bad_request"));
        assert!(err.reason.unwrap().contains("nesting"));
    }

    #[test]
    fn local_documents_follow_couchdb() {
        let local = |id: &str, rev: Option<&str>, deleted: bool| {
            let mut d = doc(id, rev, serde_json::json!({"v": 1}));
            d.deleted = deleted;
            d.attachments.insert("a".into(), inline(b"x"));
            plan_local_write(d)
        };
        match local("_local/x", None, false).unwrap() {
            LocalWrite::Put { id, body, result } => {
                assert_eq!(id, "x");
                assert_eq!(body, serde_json::json!({"v": 1, "_rev": "0-1"}));
                assert_eq!(
                    (result.id.as_str(), result.rev.as_deref()),
                    ("_local/x", Some("0-1"))
                );
            }
            other => panic!("{other:?}"),
        }
        match local("_local/x", Some("0-41"), false).unwrap() {
            LocalWrite::Put { result, .. } => assert_eq!(result.rev.as_deref(), Some("0-42")),
            other => panic!("{other:?}"),
        }
        match local("_local/x", Some("0-3"), true).unwrap() {
            LocalWrite::Delete { id, result } => {
                assert_eq!((id.as_str(), result.rev.as_deref()), ("x", Some("0-0")));
            }
            other => panic!("{other:?}"),
        }
        for rev in ["1-abc", "1-5", "0-x"] {
            let err = local("_local/x", Some(rev), false).unwrap_err();
            assert_eq!(err.reason.as_deref(), Some("Invalid rev format"), "{rev}");
        }
        let err = local("_local/", None, false).unwrap_err();
        assert_eq!(err.error.as_deref(), Some("illegal_docid"));
        assert_eq!(local_doc_id("_local/a/b"), Some("a/b"));
        assert_eq!(local_doc_id("_design/a"), None);

        let got = local_document("x", serde_json::json!({"_id": "zz", "_rev": "0-7", "v": 2}));
        assert_eq!(got.id, "_local/x");
        assert_eq!(got.rev.unwrap().to_string(), "0-7");
        assert_eq!(got.data, serde_json::json!({"v": 2}));
        let got = local_document("x", serde_json::json!({"v": 2}));
        assert_eq!(got.rev.unwrap().to_string(), "0-1");
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
