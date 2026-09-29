//! Property-based tests for the revision tree algorithms (`merge`, the
//! winning revision, stemming and purge) and for the revision/document
//! codecs.
//!
//! The generator builds arbitrary revision trees (up to 12 revisions deep,
//! up to 3 children per node, short hashes so generations tie and hashes
//! are prefixes of each other, deleted revisions, compacted ancestors) and
//! decomposes them into root-to-leaf paths, optionally cut at the root the
//! way a partial `_revisions` list or a stemmed replica sends them. Every
//! `(generation, hash)` is unique within a tree, as it is for real
//! revisions (the hash covers the parent revision).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use proptest::prelude::*;
use proptest::sample::subsequence;
use rouchdb_core::document::{Document, Revision, generate_rev_hash};
use rouchdb_core::merge::{
    MergeResult, collect_conflicts, is_deleted, merge_and_stem, merge_tree, remove_leaves, stem,
    stem_revs, winning_rev,
};
use rouchdb_core::rev_tree::{
    NodeOpts, RevNode, RevPath, RevStatus, RevTree, find_rev_ancestry, root_to_leaf,
};
use serde_json::{Value, json};

/// Maximum number of revisions on a root-to-leaf path of a generated tree.
const MAX_DEPTH: u64 = 12;
/// Maximum number of children of a generated node.
const MAX_CHILDREN: usize = 3;

fn config() -> ProptestConfig {
    // 256 cases unless `PROPTEST_CASES` asks for more.
    ProptestConfig {
        failure_persistence: None,
        // Bounded shrinking keeps a failure report (and mutation runs) fast.
        max_shrink_iters: 1024,
        ..ProptestConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Tree helpers (independent of the code under test)
// ---------------------------------------------------------------------------

type Key = (u64, String);

/// Canonical rendering of the whole tree: every root with its position,
/// every node with its hash, `(m)` when missing, `(d)` when deleted, and
/// children in stored order.
fn dump(tree: &RevTree) -> String {
    fn render(n: &RevNode, pos: u64, out: &mut String) {
        out.push_str(&format!("{}-{}", pos, n.hash));
        if n.status == RevStatus::Missing {
            out.push_str("(m)");
        }
        if n.opts.deleted {
            out.push_str("(d)");
        }
        if !n.children.is_empty() {
            out.push('[');
            for (i, c) in n.children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render(c, pos + 1, out);
            }
            out.push(']');
        }
    }
    let mut out = String::new();
    for (i, p) in tree.iter().enumerate() {
        if i > 0 {
            out.push_str(" | ");
        }
        render(&p.tree, p.pos, &mut out);
    }
    out
}

/// Every node as `(generation, hash)`, with repetitions.
fn keys(tree: &RevTree) -> Vec<Key> {
    fn walk(n: &RevNode, pos: u64, out: &mut Vec<Key>) {
        out.push((pos, n.hash.clone()));
        for c in &n.children {
            walk(c, pos + 1, out);
        }
    }
    let mut out = Vec::new();
    for p in tree {
        walk(&p.tree, p.pos, &mut out);
    }
    out
}

fn key_set(tree: &RevTree) -> BTreeSet<Key> {
    keys(tree).into_iter().collect()
}

/// Every leaf as `(generation, hash, deleted)`, with repetitions.
fn leaves(tree: &RevTree) -> Vec<(u64, String, bool)> {
    fn walk(n: &RevNode, pos: u64, out: &mut Vec<(u64, String, bool)>) {
        if n.children.is_empty() {
            out.push((pos, n.hash.clone(), n.opts.deleted));
        }
        for c in &n.children {
            walk(c, pos + 1, out);
        }
    }
    let mut out = Vec::new();
    for p in tree {
        walk(&p.tree, p.pos, &mut out);
    }
    out
}

fn leaf_set(tree: &RevTree) -> BTreeSet<Key> {
    leaves(tree).into_iter().map(|(p, h, _)| (p, h)).collect()
}

fn rev(pos: u64, hash: &str) -> String {
    format!("{}-{}", pos, hash)
}

/// Linear path (root first) with the given node data.
fn chain(pos: u64, ids: &[(String, NodeOpts, RevStatus)]) -> RevPath {
    let mut node: Option<RevNode> = None;
    for (hash, opts, status) in ids.iter().rev() {
        node = Some(RevNode {
            hash: hash.clone(),
            status: status.clone(),
            opts: opts.clone(),
            children: node.into_iter().collect(),
        });
    }
    RevPath {
        pos,
        tree: node.expect("non-empty path"),
    }
}

fn fold(paths: &[RevPath], rev_limit: u64) -> RevTree {
    paths
        .iter()
        .fold(Vec::new(), |tree, p| merge_tree(&tree, p, rev_limit).0)
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// One generated revision: where to attach it and its data.
#[derive(Debug, Clone)]
struct NodeSpec {
    hash: String,
    /// Prefer the previously generated node as the parent (deep trees).
    extend_last: bool,
    parent: usize,
    deleted: bool,
    missing: bool,
}

fn node_spec() -> impl Strategy<Value = NodeSpec> {
    (
        "[0-9a-f]{1,2}",
        prop::bool::weighted(0.6),
        any::<usize>(),
        prop::bool::weighted(0.2),
        any::<bool>(),
    )
        .prop_map(|(hash, extend_last, parent, deleted, missing)| NodeSpec {
            hash,
            extend_last,
            parent,
            deleted,
            missing,
        })
}

struct Arena {
    hash: String,
    depth: u64,
    children: Vec<usize>,
    deleted: bool,
    missing: bool,
}

/// An arbitrary single-root revision tree. Children are ordered by hash
/// (the order `merge` keeps them in), deleted flags are arbitrary, inner
/// revisions are missing (compacted) about half of the time and leaves are
/// available.
fn arb_tree() -> impl Strategy<Value = RevTree> {
    (
        1u64..=4,
        node_spec(),
        prop::collection::vec(node_spec(), 0..40),
    )
        .prop_map(|(pos, root, specs)| {
            let mut arena = vec![Arena {
                hash: root.hash,
                depth: 0,
                children: vec![],
                deleted: root.deleted,
                missing: root.missing,
            }];
            let mut used: BTreeSet<Key> = BTreeSet::new();
            used.insert((0, arena[0].hash.clone()));
            let mut last = 0;
            for s in specs {
                let open: Vec<usize> = (0..arena.len())
                    .filter(|&i| {
                        arena[i].children.len() < MAX_CHILDREN && arena[i].depth + 1 < MAX_DEPTH
                    })
                    .collect();
                if open.is_empty() {
                    break;
                }
                let parent = if s.extend_last && open.contains(&last) {
                    last
                } else {
                    open[s.parent % open.len()]
                };
                let depth = arena[parent].depth + 1;
                if !used.insert((depth, s.hash.clone())) {
                    continue;
                }
                arena.push(Arena {
                    hash: s.hash,
                    depth,
                    children: vec![],
                    deleted: s.deleted,
                    missing: s.missing,
                });
                last = arena.len() - 1;
                arena[parent].children.push(last);
            }
            fn build(arena: &[Arena], i: usize) -> RevNode {
                let a = &arena[i];
                let mut children: Vec<RevNode> =
                    a.children.iter().map(|&c| build(arena, c)).collect();
                children.sort_by(|x, y| x.hash.cmp(&y.hash));
                RevNode {
                    hash: a.hash.clone(),
                    status: if a.missing && !children.is_empty() {
                        RevStatus::Missing
                    } else {
                        RevStatus::Available
                    },
                    opts: NodeOpts { deleted: a.deleted },
                    children,
                }
            }
            vec![RevPath {
                pos,
                tree: build(&arena, 0),
            }]
        })
}

/// A tree with its root-to-leaf paths, each one cut at the root (keeping at
/// least the leaf) about a third of the time.
fn arb_tree_and_paths() -> impl Strategy<Value = (RevTree, Vec<RevPath>)> {
    arb_tree().prop_flat_map(|tree| {
        let n = root_to_leaf(&tree).len();
        (
            Just(tree),
            prop::collection::vec((prop::bool::weighted(0.35), any::<usize>()), n),
        )
            .prop_map(|(tree, cuts)| {
                let paths = root_to_leaf(&tree)
                    .into_iter()
                    .zip(cuts)
                    .map(|((pos, ids), (cut, at))| {
                        let skip = if cut { at % ids.len() } else { 0 };
                        chain(pos + skip as u64, &ids[skip..])
                    })
                    .collect();
                (tree, paths)
            })
    })
}

/// A tree, its (possibly cut) paths and two independent orderings of them.
fn arb_permuted_paths() -> impl Strategy<Value = (RevTree, Vec<RevPath>, Vec<RevPath>)> {
    arb_tree_and_paths().prop_flat_map(|(tree, paths)| {
        (
            Just(tree),
            Just(paths.clone()).prop_shuffle(),
            Just(paths).prop_shuffle(),
        )
    })
}

/// The oracle winner: the maximum leaf by (not deleted, generation, hash).
fn brute_force_winner(tree: &RevTree) -> Option<(u64, String, bool)> {
    leaves(tree)
        .into_iter()
        .max_by(|a, b| (!a.2, a.0, &a.1).cmp(&(!b.2, b.0, &b.1)))
}

fn check_winner(tree: &RevTree) -> Result<(), TestCaseError> {
    let winner = brute_force_winner(tree);
    prop_assert_eq!(
        winning_rev(tree).map(|r| (r.pos, r.hash)),
        winner.as_ref().map(|(p, h, _)| (*p, h.clone())),
        "winner of {}",
        dump(tree)
    );
    prop_assert_eq!(
        is_deleted(tree),
        winner.as_ref().is_some_and(|w| w.2),
        "is_deleted of {}",
        dump(tree)
    );
    // Conflicts: every other live leaf, newest generation first then by
    // hash, descending.
    let mut expected: Vec<(u64, String)> = leaves(tree)
        .into_iter()
        .filter(|(p, h, d)| !d && winner.as_ref().is_none_or(|w| (w.0, &w.1) != (*p, h)))
        .map(|(p, h, _)| (p, h))
        .collect();
    expected.sort_by(|a, b| b.cmp(a));
    let got: Vec<(u64, String)> = collect_conflicts(tree)
        .into_iter()
        .map(|r| (r.pos, r.hash))
        .collect();
    prop_assert_eq!(got, expected, "conflicts of {}", dump(tree));
    Ok(())
}

// ---------------------------------------------------------------------------
// P1-P3: merge is order independent, idempotent, and picks the right winner
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// P1: merging the paths of a tree in any order builds exactly the same
    /// tree. Uncut paths rebuild the original tree; every leaf appears once.
    #[test]
    fn merge_is_order_independent((tree, a, b) in arb_permuted_paths()) {
        let from_a = fold(&a, 0);
        let from_b = fold(&b, 0);
        prop_assert_eq!(dump(&from_a), dump(&from_b));

        let all: Vec<Key> = leaves(&from_a).into_iter().map(|(p, h, _)| (p, h)).collect();
        let unique: BTreeSet<Key> = all.iter().cloned().collect();
        prop_assert_eq!(all.len(), unique.len(), "duplicate leaf in {}", dump(&from_a));
        prop_assert_eq!(unique, leaf_set(&tree));
        let all_keys = keys(&from_a);
        prop_assert_eq!(
            all_keys.len(),
            key_set(&from_a).len(),
            "duplicate revision in {}",
            dump(&from_a)
        );
        let path_keys: BTreeSet<Key> = a.iter().flat_map(|p| keys(&vec![p.clone()])).collect();
        prop_assert_eq!(key_set(&from_a), path_keys);

        let full: Vec<RevPath> = root_to_leaf(&tree)
            .into_iter()
            .map(|(pos, ids)| chain(pos, &ids))
            .collect();
        prop_assert_eq!(dump(&fold(&full, 0)), dump(&tree));
    }

    /// P2: merging a path twice is the same as merging it once, and merging
    /// a path the tree already holds changes nothing.
    #[test]
    fn merge_is_idempotent(
        (tree, paths, _) in arb_permuted_paths(),
        split in any::<prop::sample::Index>(),
        pick in any::<prop::sample::Index>(),
    ) {
        let partial = fold(&paths[..split.index(paths.len() + 1)], 0);
        let p = pick.get(&paths);
        let (once, _) = merge_tree(&partial, p, 0);
        let (twice, again) = merge_tree(&once, p, 0);
        prop_assert_eq!(dump(&twice), dump(&once));
        prop_assert_eq!(again, MergeResult::InternalNode);

        let (same, result) = merge_tree(&tree, p, 0);
        prop_assert_eq!(dump(&same), dump(&tree));
        prop_assert_eq!(result, MergeResult::InternalNode);
    }

    /// P3: `winning_rev`, `is_deleted` and `collect_conflicts` agree with a
    /// brute-force maximum over the leaves, whatever the merge order.
    #[test]
    fn winner_matches_brute_force((tree, a, b) in arb_permuted_paths()) {
        check_winner(&tree)?;
        let from_a = fold(&a, 0);
        let from_b = fold(&b, 0);
        check_winner(&from_a)?;
        prop_assert_eq!(winning_rev(&from_a), winning_rev(&from_b));
        prop_assert_eq!(winning_rev(&from_a), winning_rev(&tree));
        prop_assert_eq!(is_deleted(&from_a), is_deleted(&from_b));
        prop_assert_eq!(collect_conflicts(&from_a), collect_conflicts(&from_b));
        prop_assert_eq!(collect_conflicts(&from_a), collect_conflicts(&tree));
    }
}

// ---------------------------------------------------------------------------
// P4-P5: stemming
// ---------------------------------------------------------------------------

fn check_stem(original: &RevTree, limit: u64) -> Result<(), TestCaseError> {
    let mut stemmed_tree = original.clone();
    let removed = stem_revs(&mut stemmed_tree, limit);
    let shown = dump(original);

    // Leaves are kept, once each.
    let got_leaves = leaves(&stemmed_tree);
    prop_assert_eq!(got_leaves.len(), leaf_set(&stemmed_tree).len());
    prop_assert_eq!(
        leaf_set(&stemmed_tree),
        leaf_set(original),
        "leaves of {}",
        shown
    );

    // Every path is at most `limit` revisions long, and each leaf keeps
    // exactly its newest `min(limit, len)` revisions.
    for (_, ids) in root_to_leaf(&stemmed_tree) {
        prop_assert!(
            ids.len() as u64 <= limit.max(1),
            "path too long in {}",
            shown
        );
    }
    for (pos, hash, _) in leaves(original) {
        let full = find_rev_ancestry(original, pos, &hash).expect("leaf of the original");
        let keep = full.len().min(limit.max(1) as usize);
        prop_assert_eq!(
            find_rev_ancestry(&stemmed_tree, pos, &hash),
            Some(full[..keep].to_vec()),
            "ancestry of {} stemmed to {} in {}",
            rev(pos, &hash),
            limit,
            shown
        );
    }

    // Removed and kept revisions partition the original ones.
    let removed_keys: Vec<Key> = removed.iter().map(|r| (r.pos, r.hash.clone())).collect();
    let removed_set: BTreeSet<Key> = removed_keys.iter().cloned().collect();
    prop_assert_eq!(
        removed_keys.len(),
        removed_set.len(),
        "duplicates in {:?}",
        removed_keys
    );
    let kept = key_set(&stemmed_tree);
    prop_assert!(
        kept.is_disjoint(&removed_set),
        "removed revisions kept in {}",
        shown
    );
    let union: BTreeSet<Key> = kept.union(&removed_set).cloned().collect();
    prop_assert_eq!(union, key_set(original));

    // `stem` reports the same revisions by hash.
    let mut by_hash = original.clone();
    let hashes = stem(&mut by_hash, limit);
    prop_assert_eq!(dump(&by_hash), dump(&stemmed_tree));
    prop_assert_eq!(
        hashes,
        removed.iter().map(|r| r.hash.clone()).collect::<Vec<_>>()
    );

    // Stemming is idempotent and never changes the winner or conflicts.
    let mut again = stemmed_tree.clone();
    prop_assert!(stem_revs(&mut again, limit).is_empty());
    prop_assert_eq!(dump(&again), dump(&stemmed_tree));
    prop_assert_eq!(winning_rev(&stemmed_tree), winning_rev(original));
    prop_assert_eq!(is_deleted(&stemmed_tree), is_deleted(original));
    prop_assert_eq!(
        collect_conflicts(&stemmed_tree),
        collect_conflicts(original)
    );
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    /// P4: stemming keeps every leaf with its newest revisions, cuts every
    /// path to the limit, and reports exactly the revisions it removed.
    #[test]
    fn stem_invariants((tree, paths, _) in arb_permuted_paths(), limit in 0u64..=13) {
        check_stem(&tree, limit)?;
        // A multi-root tree, as left behind by partial `_revisions`.
        check_stem(&fold(&paths, 0), limit)?;
    }

    /// P5: merging with a revision limit is merging without one, then
    /// stemming to the limit.
    #[test]
    fn merge_with_limit_is_merge_then_stem(
        (_, paths, _) in arb_permuted_paths(),
        limit in 1u64..=13,
        split in any::<prop::sample::Index>(),
        pick in any::<prop::sample::Index>(),
    ) {
        // The existing tree was itself stored with the limit.
        let existing = fold(&paths[..split.index(paths.len() + 1)], limit);
        let p = pick.get(&paths);

        let (limited, result, removed) = merge_and_stem(&existing, p, limit);
        let (mut unlimited, unlimited_result) = merge_tree(&existing, p, 0);
        let before_stem = key_set(&unlimited);
        stem_revs(&mut unlimited, limit);
        prop_assert_eq!(dump(&limited), dump(&unlimited));
        prop_assert_eq!(result, unlimited_result);
        // The removed revisions are those of the merged tree the limit cut.
        let removed: BTreeSet<Key> = removed.into_iter().map(|r| (r.pos, r.hash)).collect();
        let cut: BTreeSet<Key> = before_stem.difference(&key_set(&limited)).cloned().collect();
        prop_assert_eq!(removed, cut);
        prop_assert_eq!(dump(&merge_tree(&existing, p, limit).0), dump(&limited));
    }
}

// ---------------------------------------------------------------------------
// P6: purge (remove_leaves)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// P6: removing leaves keeps exactly the other leaves with their full
    /// ancestry and drops every revision no remaining leaf needs.
    #[test]
    fn remove_leaves_semantics(
        (tree, paths, _) in arb_permuted_paths(),
        which in any::<prop::sample::Index>(),
        chosen in subsequence((0..64usize).collect::<Vec<_>>(), 0..8),
    ) {
        let tree = if which.index(2) == 0 { tree } else { fold(&paths, 0) };
        let all_leaves: Vec<String> = leaves(&tree).iter().map(|(p, h, _)| rev(*p, h)).collect();
        let inner: Vec<String> = keys(&tree)
            .into_iter()
            .map(|(p, h)| rev(p, &h))
            .filter(|r| !all_leaves.contains(r))
            .collect();
        // Leaves, inner revisions, an unknown revision and a duplicate.
        let mut pool: Vec<String> = all_leaves.clone();
        pool.extend(inner);
        pool.push("99-unknown".to_string());
        let mut requested: Vec<String> = chosen.iter().map(|&i| pool[i % pool.len()].clone()).collect();
        if let Some(first) = requested.first().cloned() {
            requested.push(first);
        }

        let (result, removed) = remove_leaves(&tree, &requested);
        let mut expected_removed: Vec<String> = Vec::new();
        for r in &requested {
            if all_leaves.contains(r) && !expected_removed.contains(r) {
                expected_removed.push(r.clone());
            }
        }
        prop_assert_eq!(&removed, &expected_removed);

        let remaining: BTreeSet<Key> = leaves(&tree)
            .into_iter()
            .map(|(p, h, _)| (p, h))
            .filter(|(p, h)| !removed.contains(&rev(*p, h)))
            .collect();
        prop_assert_eq!(leaf_set(&result), remaining.clone());
        // Exactly the ancestry of the remaining leaves survives, unchanged.
        let mut needed: BTreeSet<Key> = BTreeSet::new();
        for (pos, hash) in &remaining {
            let ancestry = find_rev_ancestry(&tree, *pos, hash).expect("leaf");
            prop_assert_eq!(find_rev_ancestry(&result, *pos, hash), Some(ancestry.clone()));
            for (i, h) in ancestry.iter().enumerate() {
                needed.insert((pos - i as u64, h.clone()));
            }
        }
        prop_assert_eq!(key_set(&result), needed);
        if removed.is_empty() {
            prop_assert_eq!(dump(&result), dump(&tree));
        }

        let (emptied, _) = remove_leaves(&tree, &all_leaves);
        prop_assert!(emptied.is_empty(), "left {}", dump(&emptied));
    }
}

// ---------------------------------------------------------------------------
// P7-P8: revision hashes and codecs
// ---------------------------------------------------------------------------

/// A small JSON value, biased towards near-identical bodies.
fn small_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (-3i64..=12).prop_map(Value::from),
        prop::sample::select(vec![0.5, -0.0, 1e300]).prop_map(|f| json!(f)),
        "[a-c01\"\\\\]{0,3}".prop_map(Value::String),
    ];
    leaf.prop_recursive(2, 12, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Value::Array),
            prop::collection::btree_map("[a-c_]{0,2}", inner, 0..3)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// A document body: an object with no member the document codec reserves.
fn body() -> impl Strategy<Value = serde_json::Map<String, Value>> {
    prop::collection::btree_map("_?[a-c]{0,2}", small_json(), 0..4).prop_map(|m| {
        m.into_iter()
            .filter(|(k, _)| !["_id", "_rev", "_deleted", "_attachments"].contains(&k.as_str()))
            .collect()
    })
}

/// Attachment entries as a stub parser input: name -> (digest, type).
fn attachment_specs() -> impl Strategy<Value = BTreeMap<String, (String, String)>> {
    prop::collection::btree_map("[ab.]{0,2}", ("(md5-)?[AB=]{0,2}", "[a/b]{0,3}"), 0..3)
}

fn stub_attachments(
    specs: &BTreeMap<String, (String, String)>,
) -> HashMap<String, rouchdb_core::document::AttachmentMeta> {
    let atts: serde_json::Map<String, Value> = specs
        .iter()
        .map(|(name, (digest, ct))| {
            (
                name.clone(),
                json!({"stub": true, "digest": digest, "content_type": ct}),
            )
        })
        .collect();
    Document::from_json(json!({ "_attachments": atts }))
        .expect("stub attachments parse")
        .attachments
}

type HashInput = (
    bool,
    Option<String>,
    String,
    BTreeMap<String, (String, String)>,
);

proptest! {
    #![proptest_config(config())]

    /// P7: distinct (deleted, parent, body, attachment name/digest/type)
    /// inputs never produce the same revision hash; `revpos` is ignored.
    #[test]
    fn rev_hash_is_injective(
        inputs in prop::collection::vec(
            (
                any::<bool>(),
                prop::option::of((1u64..=12, "[0-9a-f]{1,3}")),
                body(),
                attachment_specs(),
            ),
            1..48,
        )
    ) {
        let mut seen: HashMap<String, HashInput> = HashMap::new();
        for (deleted, prev, body, atts) in inputs {
            let prev = prev.map(|(p, h)| rev(p, &h));
            let body = Value::Object(body);
            let hash = generate_rev_hash(&body, deleted, prev.as_deref(), &stub_attachments(&atts));
            prop_assert_eq!(hash.len(), 32);
            prop_assert!(hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            let input: HashInput = (deleted, prev.clone(), body.to_string(), atts.clone());
            if let Some(other) = seen.get(&hash) {
                prop_assert_eq!(other, &input, "hash collision {}", hash);
            }
            seen.insert(hash.clone(), input);

            // `revpos` is not part of the hash: the same content uploaded at
            // another generation hashes the same.
            let mut moved = stub_attachments(&atts);
            for meta in moved.values_mut() {
                meta.revpos += 7;
            }
            prop_assert_eq!(
                generate_rev_hash(&body, deleted, prev.as_deref(), &moved),
                hash
            );
        }
    }

    /// P8a: a revision displays as `pos-hash` and parses back to itself in
    /// its normalized form (32-digit hex ids are lower-cased).
    #[test]
    fn revision_display_parse_round_trip(
        pos in any::<u64>(),
        hash in prop_oneof!["[0-9a-fA-F]{32}", "[0-9a-zA-Z_.-]{1,40}"],
    ) {
        let revision = Revision::new(pos, hash.clone());
        let text = revision.to_string();
        prop_assert_eq!(&text, &format!("{}-{}", pos, hash));
        let parsed: Revision = text.parse().expect("valid revision");
        prop_assert_eq!(&parsed, &revision.clone().normalized());
        let is_hex32 = hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        let expected_hash = if is_hex32 { hash.to_ascii_lowercase() } else { hash };
        prop_assert_eq!(&parsed.hash, &expected_hash);
        // The normalized form is a fixed point.
        prop_assert_eq!(parsed.to_string().parse::<Revision>().expect("valid"), parsed);
    }

    /// P8b: `Document::from_json(doc.to_json())` gives the document back
    /// for bodies without reserved members.
    #[test]
    fn document_json_round_trip(
        id in "[a-zA-Z0-9_/:-]{1,10}",
        revision in prop::option::of((1u64..=1000, "[0-9a-f]{32}|[a-z]{1,3}")),
        deleted in any::<bool>(),
        body in body(),
        stubs in attachment_specs(),
        stub_extra in prop::collection::vec((0u64..4, prop::option::of(("gzip|identity", 0u64..100))), 3),
        inline in prop::collection::btree_map(
            "[cd]{1,2}",
            (prop::collection::vec(any::<u8>(), 0..8), 0u64..4),
            0..3,
        ),
    ) {
        use base64::Engine as _;
        let mut json = Value::Object(body.clone());
        json["_id"] = json!(id);
        if let Some((p, h)) = &revision {
            json["_rev"] = json!(rev(*p, h));
        }
        json["_deleted"] = json!(deleted);
        let mut atts = serde_json::Map::new();
        for ((name, (digest, ct)), (revpos, encoding)) in stubs.iter().zip(&stub_extra) {
            let mut stub = json!({"stub": true, "digest": digest, "content_type": ct, "length": 3});
            if *revpos > 0 {
                stub["revpos"] = json!(revpos);
            }
            if let Some((encoding, encoded_length)) = encoding {
                stub["encoding"] = json!(encoding);
                stub["encoded_length"] = json!(encoded_length);
            }
            atts.insert(name.clone(), stub);
        }
        for (name, (bytes, revpos)) in &inline {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            let mut att = json!({"content_type": "application/x", "data": data});
            if *revpos > 0 {
                att["revpos"] = json!(revpos);
            }
            atts.insert(name.clone(), att);
        }
        if !atts.is_empty() {
            json["_attachments"] = Value::Object(atts);
        }

        let doc = Document::from_json(json).expect("valid document");
        prop_assert_eq!(&doc.data, &Value::Object(body));
        let back = Document::from_json(doc.to_json()).expect("round trip parses");
        prop_assert_eq!(&back.id, &doc.id);
        prop_assert_eq!(&back.rev, &doc.rev);
        prop_assert_eq!(back.deleted, doc.deleted);
        prop_assert_eq!(&back.data, &doc.data);
        prop_assert_eq!(back.attachments.len(), doc.attachments.len());
        for (name, a) in &doc.attachments {
            let b = back.attachments.get(name).expect("attachment kept");
            prop_assert_eq!(&b.content_type, &a.content_type);
            prop_assert_eq!(&b.digest, &a.digest);
            prop_assert_eq!(b.length, a.length);
            prop_assert_eq!(b.stub, a.stub);
            prop_assert_eq!(&b.data, &a.data);
            prop_assert_eq!(b.revpos, a.revpos);
            prop_assert_eq!(&b.encoding, &a.encoding);
            prop_assert_eq!(b.encoded_length, a.encoded_length);
        }
    }
}
