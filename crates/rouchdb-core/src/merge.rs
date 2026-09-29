/// Revision tree merge algorithm.
///
/// Implements the same logic as PouchDB's `pouchdb-merge` module:
/// - Merge incoming revision paths into an existing tree
/// - Determine the winning revision deterministically
/// - Stem (prune) old revisions beyond a configurable limit
use crate::document::Revision;
use crate::rev_tree::{
    NodeOpts, RevNode, RevPath, RevStatus, RevTree, collect_leaves, rev_exists, root_to_leaf,
};

/// Result of merging a new path into the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    /// The path extended an existing branch (normal edit).
    NewLeaf,
    /// The path created a new branch (conflict).
    NewBranch,
    /// The path's leaf already existed in the tree (duplicate/no-op).
    InternalNode,
}

/// Merge a new revision path into the existing tree.
///
/// Returns the updated tree and a `MergeResult` indicating what happened.
/// This is a port of `pouchdb-merge`'s `merge()`: the path is merged into
/// every root it overlaps (`doMerge`), then the tree is stemmed to
/// `rev_limit` revisions per root-to-leaf path, which also collapses roots
/// that became duplicates.
pub fn merge_tree(tree: &RevTree, new_path: &RevPath, rev_limit: u64) -> (RevTree, MergeResult) {
    let (result_tree, merge_result, _stemmed) = merge_and_stem(tree, new_path, rev_limit);
    (result_tree, merge_result)
}

/// [`merge_tree`], also returning the revisions stemmed out of the tree by
/// `rev_limit` (a `rev_limit` of 0 means no limit). Like CouchDB, a stemmed
/// revision no longer exists: adapters must drop its stored body.
pub fn merge_and_stem(
    tree: &RevTree,
    new_path: &RevPath,
    rev_limit: u64,
) -> (RevTree, MergeResult, Vec<Revision>) {
    let (mut result_tree, merge_result) = do_merge(tree, new_path, false);

    // Always re-normalize through stem (an unlimited depth when there is no
    // rev_limit) so overlapping roots merged above are collapsed into one.
    let depth = if rev_limit > 0 { rev_limit } else { u64::MAX };
    let stemmed = stem_revs(&mut result_tree, depth);

    (result_tree, merge_result, stemmed)
}

/// Core merge logic (`doMerge` in pouchdb-merge).
///
/// `new_path` is merged into every existing root it overlaps, whether the
/// overlap is at the same root, deeper in an existing root, or deeper in the
/// incoming path (in which case the incoming path becomes the new root and
/// keeps its older ancestors). When `dont_expand` is set (used while
/// re-merging stemmed paths), only roots starting at the same revision merge.
fn do_merge(tree: &RevTree, new_path: &RevPath, dont_expand: bool) -> (RevTree, MergeResult) {
    if tree.is_empty() {
        return (vec![new_path.clone()], MergeResult::NewLeaf);
    }

    let mut restree: RevTree = Vec::with_capacity(tree.len() + 1);
    let mut conflicts: Option<MergeResult> = None;
    let mut merged = false;
    // The incoming path absorbs existing roots that start below its root, so
    // later roots are compared against the grown path.
    let mut path = new_path.clone();

    for branch in tree {
        if branch.pos == path.pos && branch.tree.hash == path.tree.hash {
            // Same root: merge the two trees node by node.
            let mut branch = branch.clone();
            let res = merge_nodes(&mut branch.tree, &path.tree);
            conflicts = conflicts.or(res);
            restree.push(branch);
            merged = true;
        } else if !dont_expand && branch.pos < path.pos {
            // The incoming path starts deeper: find its root inside the branch.
            let mut branch = branch.clone();
            if let Some(target) =
                find_at_depth_mut(&mut branch.tree, path.pos - branch.pos, &path.tree.hash)
            {
                let res = merge_nodes(target, &path.tree);
                conflicts = conflicts.or(res);
                merged = true;
            }
            restree.push(branch);
        } else if !dont_expand && branch.pos > path.pos {
            // The existing branch starts deeper: graft it into the incoming
            // path, which becomes the root and keeps its older ancestors.
            let diff = branch.pos - path.pos;
            match find_at_depth_mut(&mut path.tree, diff, &branch.tree.hash) {
                Some(target) => {
                    // Classify from the incoming side: what does the path add
                    // below the branch root?
                    let mut probe = branch.tree.clone();
                    let res = merge_nodes(&mut probe, target);
                    merge_nodes(target, &branch.tree);
                    conflicts = conflicts.or(res);
                    restree.push(path.clone());
                    merged = true;
                }
                None => restree.push(branch.clone()),
            }
        } else {
            restree.push(branch.clone());
        }
    }

    if !merged {
        // No overlap with any root: a disjoint new root, i.e. a new branch.
        restree.push(path);
        conflicts = Some(MergeResult::NewBranch);
    }

    restree.sort_by_key(|p| p.pos);

    (restree, conflicts.unwrap_or(MergeResult::InternalNode))
}

/// Merge `incoming` into `existing` (`mergeTree` in pouchdb-merge). Both nodes
/// must be the same revision. Returns what the merge added, if anything.
fn merge_nodes(existing: &mut RevNode, incoming: &RevNode) -> Option<MergeResult> {
    let mut conflicts = None;
    merge_nodes_into(existing, incoming, &mut conflicts);
    conflicts
}

fn merge_nodes_into(
    existing: &mut RevNode,
    incoming: &RevNode,
    conflicts: &mut Option<MergeResult>,
) {
    // A revision is available if either side has its body.
    if incoming.status == RevStatus::Available {
        existing.status = RevStatus::Available;
    }

    for child in &incoming.children {
        if existing.children.is_empty() {
            // Extending a leaf.
            *conflicts = Some(MergeResult::NewLeaf);
            existing.children.push(child.clone());
            continue;
        }

        let mut found = false;
        for existing_child in existing.children.iter_mut() {
            if existing_child.hash == child.hash {
                merge_nodes_into(existing_child, child, conflicts);
                found = true;
            }
        }
        if !found {
            // A sibling of existing children: a new conflicting branch.
            *conflicts = Some(MergeResult::NewBranch);
            let idx = existing
                .children
                .partition_point(|c| c.hash.as_str() < child.hash.as_str());
            existing.children.insert(idx, child.clone());
        }
    }
}

/// Find the node exactly `depth` levels below `node` whose hash is `hash`.
fn find_at_depth_mut<'a>(node: &'a mut RevNode, depth: u64, hash: &str) -> Option<&'a mut RevNode> {
    if depth == 0 {
        return if node.hash == hash { Some(node) } else { None };
    }
    for child in node.children.iter_mut() {
        if let Some(found) = find_at_depth_mut(child, depth - 1, hash) {
            return Some(found);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Winning revision
// ---------------------------------------------------------------------------

/// Determine the winning revision of a document.
///
/// CouchDB's deterministic algorithm:
/// 1. Non-deleted leaves win over deleted leaves
/// 2. Higher position (generation) wins
/// 3. Lexicographically greater hash breaks ties
///
/// Every replica independently arrives at the same winner.
pub fn winning_rev(tree: &RevTree) -> Option<Revision> {
    let leaves = collect_leaves(tree);
    leaves.first().map(|l| Revision::new(l.pos, l.hash.clone()))
}

/// Check if the document's winning revision is deleted.
pub fn is_deleted(tree: &RevTree) -> bool {
    collect_leaves(tree)
        .first()
        .map(|l| l.deleted)
        .unwrap_or(false)
}

/// Collect all conflicting (non-winning, non-deleted) leaf revisions.
pub fn collect_conflicts(tree: &RevTree) -> Vec<Revision> {
    let leaves = collect_leaves(tree);
    leaves
        .iter()
        .skip(1) // skip the winner
        .filter(|l| !l.deleted)
        .map(|l| Revision::new(l.pos, l.hash.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Stemming (pruning old revisions)
// ---------------------------------------------------------------------------

/// Maximum number of edges from `node` to its deepest descendant leaf.
#[cfg(test)]
fn max_depth(node: &RevNode) -> u64 {
    if node.children.is_empty() {
        return 0;
    }
    node.children
        .iter()
        .map(|c| 1 + max_depth(c))
        .max()
        .unwrap_or(0)
}

/// Prune revisions so every root-to-leaf path keeps at most `depth`
/// revisions. Returns the list of revision hashes that were removed.
///
/// This is a port of `pouchdb-merge`'s `stem()`: the tree is decomposed into
/// root-to-leaf paths, each path is cut independently, and the cut paths are
/// merged back together. A shared ancestor is therefore only removed when no
/// remaining path still needs it (a short branch keeps its full ancestry even
/// when a sibling branch is deep). This is why a `RevTree` is a list of roots.
pub fn stem(tree: &mut RevTree, depth: u64) -> Vec<String> {
    stem_revs(tree, depth)
        .into_iter()
        .map(|rev| rev.hash)
        .collect()
}

/// [`stem`], returning the removed revisions with their generation.
pub fn stem_revs(tree: &mut RevTree, depth: u64) -> Vec<Revision> {
    let depth = depth.max(1);
    let mut stemmed: Vec<(u64, String)> = Vec::new();
    let mut result: RevTree = Vec::new();

    for (pos, ids) in root_to_leaf(tree) {
        let len = ids.len() as u64;
        let cut = len.saturating_sub(depth) as usize;
        for (i, (hash, _, _)) in ids.iter().take(cut).enumerate() {
            let rev = (pos + i as u64, hash.clone());
            if !stemmed.contains(&rev) {
                stemmed.push(rev);
            }
        }
        let path = RevPath {
            pos: pos + cut as u64,
            tree: path_to_tree(&ids[cut..]),
        };
        if is_empty_node(&path.tree) {
            continue;
        }
        result = if result.is_empty() {
            vec![path]
        } else {
            do_merge(&result, &path, true).0
        };
    }

    // A revision removed from one path may still live on another one.
    stemmed.retain(|(pos, hash)| !rev_exists(&result, *pos, hash));

    *tree = result;
    stemmed
        .into_iter()
        .map(|(pos, hash)| Revision::new(pos, hash))
        .collect()
}

/// Rebuild a linear chain (root first) produced by `root_to_leaf`.
fn path_to_tree(ids: &[(String, NodeOpts, RevStatus)]) -> RevNode {
    let mut node: Option<RevNode> = None;
    for (hash, opts, status) in ids.iter().rev() {
        node = Some(RevNode {
            hash: hash.clone(),
            status: status.clone(),
            opts: opts.clone(),
            children: node.into_iter().collect(),
        });
    }
    node.unwrap_or(RevNode {
        hash: String::new(),
        status: RevStatus::Missing,
        opts: NodeOpts::default(),
        children: vec![],
    })
}

fn is_empty_node(node: &RevNode) -> bool {
    node.hash.is_empty() && node.children.is_empty()
}

/// Remove the given leaf revisions from the tree, together with every
/// ancestor that no remaining leaf still needs (`couch_key_tree:remove_leafs`,
/// used by purge). Revisions that are not leaves are ignored.
///
/// Returns the new tree and the revisions actually removed as leaves.
pub fn remove_leaves(tree: &RevTree, revs: &[String]) -> (RevTree, Vec<String>) {
    let leaves: Vec<String> = collect_leaves(tree)
        .iter()
        .map(|l| l.rev_string())
        .collect();
    let mut removed: Vec<String> = Vec::new();
    for rev in revs {
        if leaves.contains(rev) && !removed.contains(rev) {
            removed.push(rev.clone());
        }
    }
    if removed.is_empty() {
        return (tree.clone(), removed);
    }

    let mut result: RevTree = Vec::new();
    for (pos, ids) in root_to_leaf(tree) {
        let leaf = format!("{}-{}", pos + ids.len() as u64 - 1, ids[ids.len() - 1].0);
        if removed.contains(&leaf) {
            continue;
        }
        let path = RevPath {
            pos,
            tree: path_to_tree(&ids),
        };
        result = if result.is_empty() {
            vec![path]
        } else {
            do_merge(&result, &path, true).0
        };
    }
    (result, removed)
}

/// Find the winning leaf revision that descends from `(pos, hash)`.
///
/// Used for `latest=true`: walk to the tip of the requested rev's branch.
/// If the rev is itself a leaf it is returned; among multiple descendant
/// leaves the deterministic winner (non-deleted > higher generation > higher
/// hash) is chosen. Returns `None` if the rev is not present in the tree.
pub fn latest_leaf(tree: &RevTree, pos: u64, hash: &str) -> Option<Revision> {
    fn find_node<'a>(
        node: &'a RevNode,
        cur: u64,
        tpos: u64,
        thash: &str,
    ) -> Option<(&'a RevNode, u64)> {
        if cur == tpos && node.hash == thash {
            return Some((node, cur));
        }
        for c in &node.children {
            if let Some(found) = find_node(c, cur + 1, tpos, thash) {
                return Some(found);
            }
        }
        None
    }

    fn collect(node: &RevNode, pos: u64, out: &mut Vec<(u64, String, bool)>) {
        if node.children.is_empty() {
            out.push((pos, node.hash.clone(), node.opts.deleted));
        } else {
            for c in &node.children {
                collect(c, pos + 1, out);
            }
        }
    }

    for path in tree {
        if let Some((node, node_pos)) = find_node(&path.tree, path.pos, pos, hash) {
            let mut leaves: Vec<(u64, String, bool)> = Vec::new();
            collect(node, node_pos, &mut leaves);
            // Winner order: non-deleted first, then highest pos, then hash desc.
            leaves.sort_by(|a, b| {
                a.2.cmp(&b.2)
                    .then_with(|| b.0.cmp(&a.0))
                    .then_with(|| b.1.cmp(&a.1))
            });
            return leaves
                .into_iter()
                .next()
                .map(|(p, h, _)| Revision::new(p, h));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// revs_diff
// ---------------------------------------------------------------------------

/// CouchDB's `_revs_diff` answer for one document with revision tree
/// `tree` (`None` if the document does not exist): which of `revs` it
/// lacks, and the leaves that could be their ancestors.
///
/// Like CouchDB 3, `missing` is sorted by generation then id (duplicates
/// kept) and `possible_ancestors` lists, once and in winner order (deleted
/// leaves last), every leaf older than the newest missing revision.
/// Revisions are parsed and normalized (upper-case hex ids are the same
/// revisions); a malformed one is an error. Returns `None` when nothing is
/// missing.
pub fn revs_diff_one(
    tree: Option<&RevTree>,
    revs: &[String],
) -> crate::error::Result<Option<crate::document::RevsDiffResult>> {
    let mut missing = Vec::new();
    for rev in revs {
        let rev: Revision = rev.parse()?;
        if !tree.is_some_and(|t| rev_exists(t, rev.pos, &rev.hash)) {
            missing.push(rev);
        }
    }
    let Some(newest) = missing.iter().map(|r| r.pos).max() else {
        return Ok(None);
    };
    missing.sort();
    let possible_ancestors = tree
        .map(|t| {
            collect_leaves(t)
                .iter()
                .filter(|leaf| leaf.pos < newest)
                .map(|leaf| leaf.rev_string())
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(crate::document::RevsDiffResult {
        missing: missing.iter().map(|r| r.to_string()).collect(),
        possible_ancestors,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rev_tree::{RevNode, RevPath, build_path_from_revs, find_rev_ancestry};

    fn leaf(hash: &str) -> RevNode {
        RevNode {
            hash: hash.into(),
            status: RevStatus::Available,
            opts: NodeOpts::default(),
            children: vec![],
        }
    }

    fn deleted_leaf(hash: &str) -> RevNode {
        RevNode {
            hash: hash.into(),
            status: RevStatus::Available,
            opts: NodeOpts { deleted: true },
            children: vec![],
        }
    }

    fn node(hash: &str, children: Vec<RevNode>) -> RevNode {
        RevNode {
            hash: hash.into(),
            status: RevStatus::Available,
            opts: NodeOpts::default(),
            children,
        }
    }

    /// Newest-first path ending in an available leaf (ancestors missing),
    /// like a replicated `_revisions` list.
    fn path(pos: u64, revs: &[&str]) -> RevPath {
        let revs: Vec<String> = revs.iter().map(|r| r.to_string()).collect();
        build_path_from_revs(pos, &revs, NodeOpts::default(), RevStatus::Available)
    }

    /// Canonical rendering of the WHOLE tree: every root with its position,
    /// every node with its hash, `(m)` when its body is missing, `(d)` when
    /// it is deleted, and the children in stored order. Comparing dumps
    /// checks the exact structure instead of a few derived facts.
    fn dump(tree: &RevTree) -> String {
        fn render(n: &RevNode, pos: u64) -> String {
            let mut s = format!("{}-{}", pos, n.hash);
            if n.status == RevStatus::Missing {
                s.push_str("(m)");
            }
            if n.opts.deleted {
                s.push_str("(d)");
            }
            if !n.children.is_empty() {
                let kids: Vec<String> = n.children.iter().map(|c| render(c, pos + 1)).collect();
                s.push_str(&format!("[{}]", kids.join(",")));
            }
            s
        }
        tree.iter()
            .map(|p| render(&p.tree, p.pos))
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn revs(v: &[Revision]) -> Vec<String> {
        v.iter().map(|r| r.to_string()).collect()
    }

    fn simple_tree() -> RevTree {
        // 1-a -> 2-b -> 3-c
        vec![RevPath {
            pos: 1,
            tree: node("a", vec![node("b", vec![leaf("c")])]),
        }]
    }

    // --- winning_rev ---

    #[test]
    fn winning_rev_simple() {
        let tree = simple_tree();
        let winner = winning_rev(&tree).unwrap();
        assert_eq!(winner.pos, 3);
        assert_eq!(winner.hash, "c");
    }

    #[test]
    fn winning_rev_conflict_picks_higher_hash() {
        // 1-a -> 2-b
        //     -> 2-c
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b"), leaf("c")]),
        }];
        let winner = winning_rev(&tree).unwrap();
        assert_eq!(winner.to_string(), "2-c"); // "c" > "b" lexicographically
    }

    #[test]
    fn winning_rev_conflict_prefers_longer() {
        // 1-a -> 2-b -> 3-d
        //     -> 2-c
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![node("b", vec![leaf("d")]), leaf("c")]),
        }];
        let winner = winning_rev(&tree).unwrap();
        assert_eq!(winner.to_string(), "3-d"); // pos 3 beats pos 2
    }

    #[test]
    fn winning_rev_non_deleted_beats_deleted() {
        // 1-a -> 2-b (non-deleted)
        //     -> 2-z (deleted) — z > b but deleted loses
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b"), deleted_leaf("z")]),
        }];
        let winner = winning_rev(&tree).unwrap();
        assert_eq!(winner.to_string(), "2-b");
        assert!(!is_deleted(&tree));
    }

    #[test]
    fn winning_rev_live_beats_deleted_of_higher_generation() {
        // 1-a -> 2-b -> 3-x (deleted)
        //     -> 2-y        (live)
        // A live leaf wins even against a deleted leaf of a higher
        // generation (CouchDB compares `deleted` before the position).
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![node("b", vec![deleted_leaf("x")]), leaf("y")]),
        }];
        assert_eq!(winning_rev(&tree).unwrap().to_string(), "2-y");
        assert!(!is_deleted(&tree));
        // The deleted leaf is not a conflict.
        assert!(collect_conflicts(&tree).is_empty());
    }

    #[test]
    fn winning_rev_all_deleted_picks_highest_generation() {
        // 1-a -> 2-z (deleted)
        //     -> 2-b -> 3-c (deleted)
        let tree = vec![RevPath {
            pos: 1,
            tree: node(
                "a",
                vec![node("b", vec![deleted_leaf("c")]), deleted_leaf("z")],
            ),
        }];
        assert_eq!(winning_rev(&tree).unwrap().to_string(), "3-c");
        assert!(is_deleted(&tree));
        assert!(collect_conflicts(&tree).is_empty());
    }

    // --- collect_conflicts ---

    #[test]
    fn no_conflicts_on_linear() {
        let tree = simple_tree();
        assert!(collect_conflicts(&tree).is_empty());
    }

    #[test]
    fn conflicts_on_branches() {
        // 1-a -> 2-b, 2-c, 2-d -> 3-e : 3-e wins, the other live leaves are
        // conflicts in winner order (higher generation, then higher hash).
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b"), leaf("c"), node("d", vec![leaf("e")])]),
        }];
        assert_eq!(winning_rev(&tree).unwrap().to_string(), "3-e");
        assert_eq!(revs(&collect_conflicts(&tree)), ["2-c", "2-b"]);
    }

    // --- is_deleted ---

    #[test]
    fn is_deleted_false_for_normal() {
        assert!(!is_deleted(&simple_tree()));
    }

    #[test]
    fn is_deleted_true_when_winner_deleted() {
        let tree = vec![RevPath {
            pos: 1,
            tree: deleted_leaf("a"),
        }];
        assert!(is_deleted(&tree));
    }

    // --- merge_tree ---

    #[test]
    fn merge_extends_linear_chain() {
        // Start: 1-a -> 2-b
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b")]),
        }];

        // Add: 3-c extending from 2-b
        let (merged, result) = merge_tree(&tree, &path(3, &["c", "b"]), 1000);
        assert_eq!(result, MergeResult::NewLeaf);
        assert_eq!(dump(&merged), "1-a[2-b[3-c]]");
        assert_eq!(winning_rev(&merged).unwrap().to_string(), "3-c");
    }

    #[test]
    fn merge_creates_conflict_branch() {
        // Start: 1-a -> 2-b
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b")]),
        }];

        // Add: 2-c branching from 1-a (conflict)
        let (merged, result) = merge_tree(&tree, &path(2, &["c", "a"]), 1000);
        assert_eq!(result, MergeResult::NewBranch);
        assert_eq!(dump(&merged), "1-a[2-b,2-c]");
        assert_eq!(winning_rev(&merged).unwrap().to_string(), "2-c");
        assert_eq!(revs(&collect_conflicts(&merged)), ["2-b"]);
    }

    #[test]
    fn merge_keeps_conflict_children_sorted_by_hash() {
        // New siblings are inserted in hash order wherever they land, so the
        // stored tree (and every traversal of it) is canonical.
        let mut tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b"), leaf("d")]),
        }];
        for h in ["c", "e", "0"] {
            let (merged, result) = merge_tree(&tree, &path(2, &[h, "a"]), 1000);
            assert_eq!(result, MergeResult::NewBranch, "adding 2-{}", h);
            tree = merged;
        }
        assert_eq!(dump(&tree), "1-a[2-0,2-b,2-c,2-d,2-e]");
        // Merging the same siblings in another order gives the same tree.
        let mut other = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("e")]),
        }];
        for h in ["0", "d", "b", "c"] {
            other = merge_tree(&other, &path(2, &[h, "a"]), 1000).0;
        }
        assert_eq!(dump(&other), dump(&tree));
    }

    #[test]
    fn merge_duplicate_is_internal_node() {
        // Start: 1-a -> 2-b
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b")]),
        }];

        // Add: 2-b (already exists)
        let (merged, result) = merge_tree(&tree, &path(2, &["b", "a"]), 1000);
        assert_eq!(result, MergeResult::InternalNode);
        assert_eq!(dump(&merged), dump(&tree));
    }

    #[test]
    fn merge_disjoint_creates_new_root() {
        // Start: 1-a -> 2-b
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b")]),
        }];

        // Add: 1-x -> 2-y (completely disjoint)
        let (merged, result) = merge_tree(&tree, &path(2, &["y", "x"]), 1000);
        assert_eq!(result, MergeResult::NewBranch);
        // Two separate roots.
        assert_eq!(dump(&merged), "1-a[2-b] | 1-x(m)[2-y]");
        assert_eq!(winning_rev(&merged).unwrap().to_string(), "2-y");
        assert_eq!(revs(&collect_conflicts(&merged)), ["2-b"]);
    }

    #[test]
    fn merge_tree_applies_rev_limit() {
        // 1-a -> 2-b -> 3-c -> 4-d
        let (tree, _) = merge_tree(&Vec::new(), &path(4, &["d", "c", "b", "a"]), 1000);
        assert_eq!(dump(&tree), "1-a(m)[2-b(m)[3-c(m)[4-d]]]");

        // 5-e arrives with rev_limit 3: only the newest 3 revisions stay.
        let next = path(5, &["e", "d"]);
        let (stemmed, result) = merge_tree(&tree, &next, 3);
        assert_eq!(result, MergeResult::NewLeaf);
        assert_eq!(dump(&stemmed), "3-c(m)[4-d[5-e]]");
        assert_eq!(stemmed[0].pos, 3);
        assert_eq!(
            find_rev_ancestry(&stemmed, 5, "e").unwrap(),
            ["e", "d", "c"]
        );

        // The limit counts revisions, inclusive: 5 keeps all, 4 drops 1-a.
        assert_eq!(
            dump(&merge_tree(&tree, &next, 5).0),
            "1-a(m)[2-b(m)[3-c(m)[4-d[5-e]]]]"
        );
        assert_eq!(
            dump(&merge_tree(&tree, &next, 4).0),
            "2-b(m)[3-c(m)[4-d[5-e]]]"
        );
        // rev_limit 0 means "no limit".
        assert_eq!(
            dump(&merge_tree(&tree, &next, 0).0),
            "1-a(m)[2-b(m)[3-c(m)[4-d[5-e]]]]"
        );
    }

    // --- stem ---

    #[test]
    fn stem_prunes_old_revisions() {
        // 1-a -> 2-b -> 3-c -> 4-d -> 5-e
        let mut tree = vec![RevPath {
            pos: 1,
            tree: node(
                "a",
                vec![node("b", vec![node("c", vec![node("d", vec![leaf("e")])])])],
            ),
        }];

        let stemmed = stem(&mut tree, 3);
        assert_eq!(stemmed, ["a", "b"]);
        // The tree now starts at 3-c and keeps the leaf.
        assert_eq!(dump(&tree), "3-c[4-d[5-e]]");
    }

    #[test]
    fn stem_splits_at_branch_point() {
        // 1-a -> 2-b -> 3-c
        //            -> 3-d
        let mut tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![node("b", vec![leaf("c"), leaf("d")])]),
        }];

        // depth=1 means each leaf keeps a single revision. The shared
        // ancestors 1-a and 2-b are pruned and each leaf becomes its own root.
        let mut stemmed = stem(&mut tree, 1);
        stemmed.sort();
        assert_eq!(stemmed, ["a", "b"]);
        assert_eq!(dump(&tree), "3-c | 3-d");
    }

    #[test]
    fn stem_limits_every_path_when_branches_share_a_cut_ancestor() {
        // 1-a -> 2-b -> 3-c -> 4-d -> 5-e
        //                   -> 4-x
        // Each root-to-leaf path is cut on its own (pouchdb-merge): 5-e keeps
        // [e, d, c] and 4-x keeps [x, c, b]. Re-merging the cut paths must
        // not graft them back into a path longer than the limit.
        let mut tree = vec![RevPath {
            pos: 1,
            tree: node(
                "a",
                vec![node(
                    "b",
                    vec![node("c", vec![node("d", vec![leaf("e")]), leaf("x")])],
                )],
            ),
        }];
        let stemmed = stem(&mut tree, 3);
        assert_eq!(stemmed, ["a"]); // 2-b is still needed by 4-x
        assert_eq!(find_rev_ancestry(&tree, 5, "e").unwrap(), ["e", "d", "c"]);
        assert_eq!(find_rev_ancestry(&tree, 4, "x").unwrap(), ["x", "c", "b"]);
        for (pos, ids) in root_to_leaf(&tree) {
            assert!(ids.len() <= 3, "path from {} too long: {:?}", pos, ids);
        }
        let leaves: Vec<String> = collect_leaves(&tree)
            .iter()
            .map(|l| l.rev_string())
            .collect();
        assert_eq!(leaves, ["5-e", "4-x"]);
    }

    #[test]
    fn stem_prunes_deep_branch_above_cut_line() {
        // Regression: a conflict branch at generation 1 used to prevent any
        // stemming, letting the deep branch grow without bound.
        //   1-a -> 2-b (leaf)
        //       -> 2-c -> 3-d -> 4-e -> 5-f
        let mut tree = vec![RevPath {
            pos: 1,
            tree: node(
                "a",
                vec![
                    leaf("b"),
                    node("c", vec![node("d", vec![node("e", vec![leaf("f")])])]),
                ],
            ),
        }];

        let stemmed = stem(&mut tree, 2);
        // Stemming is per root-to-leaf path (pouchdb-merge): the short branch
        // 1-a -> 2-b already fits the limit, so 1-a survives on it and only
        // 2-c and 3-d are pruned from the deep branch.
        let mut stemmed_sorted = stemmed.clone();
        stemmed_sorted.sort();
        assert_eq!(stemmed_sorted, vec!["c".to_string(), "d".to_string()]);

        // Roots become [1-a -> 2-b] and [4-e -> 5-f].
        assert_eq!(dump(&tree), "1-a[2-b] | 4-e[5-f]");
        for path in &tree {
            assert!(max_depth(&path.tree) < 2, "every chain must fit the limit");
        }
        // Winner is still 5-f (highest generation across roots).
        let winner = winning_rev(&tree).unwrap();
        assert_eq!(winner.pos, 5);
        assert_eq!(winner.hash, "f");
        // The short branch keeps its full ancestry.
        assert_eq!(find_rev_ancestry(&tree, 2, "b").unwrap(), vec!["b", "a"]);
    }

    #[test]
    fn stem_short_tree_unchanged() {
        // 1-a -> 2-b (2 revisions, limit 3 => nothing to prune)
        let mut tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b")]),
        }];

        let stemmed = stem(&mut tree, 3);
        assert!(stemmed.is_empty());
        assert_eq!(dump(&tree), "1-a[2-b]");
    }

    // --- remove_leaves (purge) ---

    #[test]
    fn remove_leaves_drops_unshared_ancestors() {
        // 1-a -> 2-b -> 3-c : purging the only leaf empties the tree instead
        // of resurrecting 2-b.
        let (tree, removed) = remove_leaves(&simple_tree(), &["3-c".to_string()]);
        assert!(tree.is_empty());
        assert_eq!(removed, vec!["3-c"]);
    }

    #[test]
    fn remove_leaves_keeps_shared_ancestors_and_ignores_internal_nodes() {
        // 1-a -> 2-b, 2-c
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b"), leaf("c")]),
        }];
        // 1-a is not a leaf: ignored.
        let (same, removed) = remove_leaves(&tree, &["1-a".to_string()]);
        assert!(removed.is_empty());
        assert_eq!(dump(&same), "1-a[2-b,2-c]");
        // Purging the loser keeps 1-a for the winner.
        let (after, removed) = remove_leaves(&tree, &["2-b".to_string()]);
        assert_eq!(removed, vec!["2-b"]);
        assert_eq!(dump(&after), "1-a[2-c]");
    }

    #[test]
    fn remove_leaves_drops_whole_root() {
        // Two roots 1-a and 1-b: purging 1-b removes that root entirely.
        let tree = vec![
            RevPath {
                pos: 1,
                tree: leaf("a"),
            },
            RevPath {
                pos: 1,
                tree: leaf("b"),
            },
        ];
        let (after, removed) = remove_leaves(&tree, &["1-b".to_string()]);
        assert_eq!(removed, vec!["1-b"]);
        assert_eq!(dump(&after), "1-a");
    }

    // --- doMerge fidelity (F20, F21, F22) ---

    #[test]
    fn merge_into_all_overlapping_roots() {
        // Tree has two roots: [1-a -> 2-b] and a stray [3-c] (e.g. 3-c arrived
        // without _revisions). 4-d then arrives with the full ancestry
        // [d, c, b, a]: it overlaps BOTH roots, so 3-c must stop being a leaf.
        let tree = vec![
            RevPath {
                pos: 1,
                tree: node("a", vec![leaf("b")]),
            },
            RevPath {
                pos: 3,
                tree: leaf("c"),
            },
        ];
        let (merged, result) = merge_tree(&tree, &path(4, &["d", "c", "b", "a"]), 1000);
        assert_eq!(result, MergeResult::NewLeaf);
        assert_eq!(dump(&merged), "1-a[2-b[3-c[4-d]]]");
        assert!(collect_conflicts(&merged).is_empty());
    }

    #[test]
    fn merge_keeps_older_incoming_ancestors() {
        // Local tree only knows [3-c]; 4-d arrives with [d, c, b, a]. The
        // incoming path starts earlier, so it must become the root and keep
        // 1-a and 2-b instead of dropping them. The existing 3-c keeps its
        // stored (available) status.
        let tree = vec![RevPath {
            pos: 3,
            tree: leaf("c"),
        }];
        let (merged, result) = merge_tree(&tree, &path(4, &["d", "c", "b", "a"]), 1000);
        assert_eq!(result, MergeResult::NewLeaf);
        assert_eq!(dump(&merged), "1-a(m)[2-b(m)[3-c[4-d]]]");
        assert_eq!(
            find_rev_ancestry(&merged, 4, "d").unwrap(),
            vec!["d", "c", "b", "a"]
        );
    }

    #[test]
    fn merge_into_empty_tree_is_new_leaf() {
        let (merged, result) = merge_tree(&Vec::new(), &path(1, &["a"]), 1000);
        assert_eq!(result, MergeResult::NewLeaf);
        assert_eq!(dump(&merged), "1-a");
    }

    #[test]
    fn merge_existing_rev_promotes_missing_status() {
        // 1-a -> 2-b where 2-b is only known as a missing ancestor; re-sending
        // 2-b with its body is an internal-node merge that makes it available.
        let tree = vec![RevPath {
            pos: 1,
            tree: node(
                "a",
                vec![RevNode {
                    hash: "b".into(),
                    status: RevStatus::Missing,
                    opts: NodeOpts::default(),
                    children: vec![leaf("c")],
                }],
            ),
        }];
        assert_eq!(dump(&tree), "1-a[2-b(m)[3-c]]");
        let (merged, result) = merge_tree(&tree, &path(2, &["b", "a"]), 1000);
        assert_eq!(result, MergeResult::InternalNode);
        assert_eq!(dump(&merged), "1-a[2-b[3-c]]");
    }

    #[test]
    fn merge_is_idempotent_and_order_independent() {
        // Two conflicting branches merged in either order give the same tree.
        let p1 = path(3, &["c", "b", "a"]);
        let p2 = path(2, &["x", "a"]);
        let (t1, _) = merge_tree(&Vec::new(), &p1, 1000);
        let (t1, r1) = merge_tree(&t1, &p2, 1000);
        let (t2, _) = merge_tree(&Vec::new(), &p2, 1000);
        let (t2, r2) = merge_tree(&t2, &p1, 1000);
        assert_eq!(r1, MergeResult::NewBranch);
        assert_eq!(r2, MergeResult::NewBranch);
        assert_eq!(dump(&t1), "1-a(m)[2-b(m)[3-c],2-x]");
        assert_eq!(dump(&t2), dump(&t1));
        // Re-merging a path that is already present changes nothing.
        for p in [&p1, &p2] {
            let (t3, r3) = merge_tree(&t1, p, 1000);
            assert_eq!(r3, MergeResult::InternalNode);
            assert_eq!(dump(&t3), dump(&t1));
        }
    }

    #[test]
    fn latest_leaf_walks_linear_chain_to_tip() {
        // 1-a -> 2-b -> 3-c : latest from an internal node returns the leaf.
        let tree = simple_tree();
        let rev = latest_leaf(&tree, 1, "a").unwrap();
        assert_eq!(rev.pos, 3);
        assert_eq!(rev.hash, "c");
        // A leaf returns itself.
        assert_eq!(latest_leaf(&tree, 3, "c").unwrap().hash, "c");
        // Unknown revisions have no latest leaf.
        assert!(latest_leaf(&tree, 2, "zzz").is_none());
        assert!(latest_leaf(&Vec::new(), 1, "a").is_none());
    }

    #[test]
    fn latest_leaf_stays_on_requested_branch() {
        // 1-a -> 2-b -> 3-c   (losing branch)
        //     -> 2-z -> 3-d -> 4-e   (winning branch by generation)
        let tree = vec![RevPath {
            pos: 1,
            tree: node(
                "a",
                vec![
                    node("b", vec![leaf("c")]),
                    node("z", vec![node("d", vec![leaf("e")])]),
                ],
            ),
        }];
        // Global winner is 4-e, but latest from 2-b must stay on its branch.
        assert_eq!(latest_leaf(&tree, 2, "b").unwrap().to_string(), "3-c");
        assert_eq!(latest_leaf(&tree, 2, "z").unwrap().to_string(), "4-e");
    }

    // --- merge edge cases ---

    #[test]
    fn merge_exact_root_match_no_children() {
        // Tree: 1-a (single node); adding the same node is a no-op.
        let tree = vec![RevPath {
            pos: 1,
            tree: leaf("a"),
        }];
        let new_path = RevPath {
            pos: 1,
            tree: leaf("a"),
        };

        let (merged, result) = merge_tree(&tree, &new_path, 1000);
        assert_eq!(result, MergeResult::InternalNode);
        assert_eq!(dump(&merged), "1-a");
    }

    #[test]
    fn merge_same_branch_extends_deeper() {
        // Tree: 1-a -> 2-b -> 3-c
        let tree = simple_tree();

        // Add: 1-a -> 2-b -> 3-c -> 4-d (full ancestry extending leaf)
        let (merged, result) = merge_tree(&tree, &path(4, &["d", "c", "b", "a"]), 1000);
        assert_eq!(result, MergeResult::NewLeaf);
        assert_eq!(dump(&merged), "1-a[2-b[3-c[4-d]]]");
    }

    #[test]
    fn merge_and_stem_reports_stemmed_revisions() {
        // 1-a -> 2-b -> 3-c, extended by 4-d with a limit of 2 revisions.
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![node("b", vec![leaf("c")])]),
        }];
        let path = build_path_from_revs(
            4,
            &["d".into(), "c".into()],
            NodeOpts::default(),
            RevStatus::Available,
        );
        let (merged, result, stemmed) = merge_and_stem(&tree, &path, 2);
        assert_eq!(result, MergeResult::NewLeaf);
        assert_eq!(
            stemmed,
            [Revision::new(1, "a".into()), Revision::new(2, "b".into())]
        );
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].pos, merged[0].tree.hash.as_str()), (3, "c"));
        // No limit (0): nothing is stemmed.
        let (_, _, stemmed) = merge_and_stem(&tree, &path, 0);
        assert!(stemmed.is_empty());
        // Within the limit: nothing is stemmed either.
        let (_, _, stemmed) = merge_and_stem(&tree, &path, 4);
        assert!(stemmed.is_empty());
    }

    #[test]
    fn revs_diff_one_matches_couchdb() {
        // 1-a -> {2-b, 2-c (deleted)}, 1-a -> 2-x -> 3-y
        let tree = vec![RevPath {
            pos: 1,
            tree: node(
                "a",
                vec![leaf("b"), deleted_leaf("c"), node("x", vec![leaf("y")])],
            ),
        }];
        let revs = |r: &[&str]| r.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(
            revs_diff_one(Some(&tree), &revs(&["1-a", "3-y"]))
                .unwrap()
                .is_none()
        );
        let diff = revs_diff_one(Some(&tree), &revs(&["4-z", "2-q", "4-z", "3-y"]))
            .unwrap()
            .unwrap();
        assert_eq!(diff.missing, ["2-q", "4-z", "4-z"]);
        assert_eq!(diff.possible_ancestors, ["3-y", "2-b", "2-c"]);
        let diff = revs_diff_one(Some(&tree), &revs(&["3-q"]))
            .unwrap()
            .unwrap();
        assert_eq!(diff.possible_ancestors, ["2-b", "2-c"]);
        let diff = revs_diff_one(Some(&tree), &revs(&["2-q"]))
            .unwrap()
            .unwrap();
        assert!(diff.possible_ancestors.is_empty());
        let diff = revs_diff_one(None, &revs(&["1-q"])).unwrap().unwrap();
        assert_eq!((diff.missing.len(), diff.possible_ancestors.len()), (1, 0));
        assert!(revs_diff_one(Some(&tree), &revs(&["bad"])).is_err());
    }

    #[test]
    fn winning_rev_empty_tree() {
        let tree: RevTree = vec![];
        assert!(winning_rev(&tree).is_none());
    }

    #[test]
    fn is_deleted_empty_tree() {
        let tree: RevTree = vec![];
        assert!(!is_deleted(&tree));
    }

    #[test]
    fn collect_conflicts_deleted_leaves_excluded() {
        // 1-a -> 2-b (normal), 2-c (deleted)
        // Winner: 2-b, conflict: none (2-c is deleted)
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b"), deleted_leaf("c")]),
        }];
        assert_eq!(winning_rev(&tree).unwrap().to_string(), "2-b");
        assert!(collect_conflicts(&tree).is_empty());
    }
}
