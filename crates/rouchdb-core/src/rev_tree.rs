/// Revision tree data structure.
///
/// Mirrors PouchDB's `pouchdb-merge` tree representation. A document's full
/// history is a `RevTree` — a list of `RevPath` roots. Each root has a
/// starting position (`pos`) and a tree of `RevNode`s.
///
/// Multiple roots arise when revisions are stemmed (pruned) and later a
/// previously-stemmed branch is re-introduced during replication.
/// Status of a revision's stored data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevStatus {
    /// Full document data is stored for this revision.
    Available,
    /// This revision was compacted; only the hash remains.
    Missing,
}

/// A single node in the revision tree.
#[derive(Debug, Clone)]
pub struct RevNode {
    /// The hash portion of the revision id.
    pub hash: String,
    /// Whether this revision's data is still stored.
    pub status: RevStatus,
    /// Optional metadata attached to this node (e.g., `_deleted` flag).
    pub opts: NodeOpts,
    /// Child revisions. Multiple children represent a conflict branch.
    pub children: Vec<RevNode>,
}

/// Per-node metadata flags.
#[derive(Debug, Clone, Default)]
pub struct NodeOpts {
    pub deleted: bool,
}

/// A rooted path in the revision tree.
///
/// `pos` is the generation number of the root node. For example, if the
/// earliest stored revision is `3-abc`, then `pos = 3`.
#[derive(Debug, Clone)]
pub struct RevPath {
    pub pos: u64,
    pub tree: RevNode,
}

/// The complete revision tree for a document. Multiple entries occur when
/// stemming creates disjoint subtrees.
pub type RevTree = Vec<RevPath>;

/// Information about a leaf node in the tree.
#[derive(Debug, Clone)]
pub struct LeafInfo {
    pub pos: u64,
    pub hash: String,
    pub deleted: bool,
    pub status: RevStatus,
}

impl LeafInfo {
    pub fn rev_string(&self) -> String {
        format!("{}-{}", self.pos, self.hash)
    }
}

// ---------------------------------------------------------------------------
// Traversal helpers
// ---------------------------------------------------------------------------

/// Depth-first traversal of the revision tree, calling `f` for each node.
/// The callback receives `(depth_from_root, node, root_pos)`.
pub fn traverse_rev_tree<F>(tree: &RevTree, mut f: F)
where
    F: FnMut(u64, &RevNode, u64),
{
    for path in tree {
        // Stack: (node, depth_from_root)
        let mut stack: Vec<(&RevNode, u64)> = vec![(&path.tree, 0)];
        while let Some((node, depth)) = stack.pop() {
            f(path.pos + depth, node, path.pos);
            for child in &node.children {
                stack.push((child, depth + 1));
            }
        }
    }
}

/// Collect all leaf nodes (nodes with no children) from the tree.
/// Returns them sorted by: non-deleted first, then highest pos, then
/// lexicographic hash — matching CouchDB's deterministic order.
pub fn collect_leaves(tree: &RevTree) -> Vec<LeafInfo> {
    let mut leaves = Vec::new();
    traverse_rev_tree(tree, |pos, node, _root_pos| {
        if node.children.is_empty() {
            leaves.push(LeafInfo {
                pos,
                hash: node.hash.clone(),
                deleted: node.opts.deleted,
                status: node.status.clone(),
            });
        }
    });
    // Sort: non-deleted first, then by pos desc, then hash desc
    leaves.sort_by(|a, b| {
        a.deleted
            .cmp(&b.deleted)
            .then_with(|| b.pos.cmp(&a.pos))
            .then_with(|| b.hash.cmp(&a.hash))
    });
    leaves
}

/// Decompose the tree into all root-to-leaf paths.
type LeafPath = (u64, Vec<(String, NodeOpts, RevStatus)>);

pub fn root_to_leaf(tree: &RevTree) -> Vec<LeafPath> {
    let mut paths = Vec::new();
    for path in tree {
        fn walk(
            node: &RevNode,
            current: &mut Vec<(String, NodeOpts, RevStatus)>,
            paths: &mut Vec<Vec<(String, NodeOpts, RevStatus)>>,
        ) {
            current.push((node.hash.clone(), node.opts.clone(), node.status.clone()));
            if node.children.is_empty() {
                paths.push(current.clone());
            } else {
                for child in &node.children {
                    walk(child, current, paths);
                }
            }
            current.pop();
        }
        let mut current = Vec::new();
        let mut collected = Vec::new();
        walk(&path.tree, &mut current, &mut collected);
        for p in collected {
            paths.push((path.pos, p));
        }
    }
    paths
}

/// Check if a specific revision exists anywhere in the tree.
pub fn rev_exists(tree: &RevTree, pos: u64, hash: &str) -> bool {
    let mut found = false;
    traverse_rev_tree(tree, |node_pos, node, _| {
        if node_pos == pos && node.hash == hash {
            found = true;
        }
    });
    found
}

// ---------------------------------------------------------------------------
// Building paths from revision arrays (for merging incoming revisions)
// ---------------------------------------------------------------------------

/// Build a single-path `RevPath` from a list of revision hashes.
///
/// `revs` is oldest-first: `[oldest_hash, ..., newest_hash]`.
/// `pos` is the generation of the *newest* (leaf) revision.
/// `opts` are the metadata flags for the *leaf* node.
/// `status` is applied to the leaf; all ancestors are `Missing`.
pub fn build_path_from_revs(
    pos: u64,
    revs: &[String],
    opts: NodeOpts,
    status: RevStatus,
) -> RevPath {
    if revs.is_empty() {
        // Return a degenerate single-node path rather than panicking.
        return RevPath {
            pos,
            tree: RevNode {
                hash: String::new(),
                status,
                opts,
                children: vec![],
            },
        };
    }
    let len = revs.len() as u64;
    let root_pos = pos.saturating_sub(len.saturating_sub(1));

    // Build from leaf to root
    let mut node: Option<RevNode> = None;
    for (i, hash) in revs.iter().enumerate() {
        let is_leaf = i == 0;
        let n = RevNode {
            hash: hash.clone(),
            status: if is_leaf {
                status.clone()
            } else {
                RevStatus::Missing
            },
            opts: if is_leaf {
                opts.clone()
            } else {
                NodeOpts::default()
            },
            children: node.into_iter().collect(),
        };
        node = Some(n);
    }

    RevPath {
        pos: root_pos,
        tree: node.expect("node must exist after building from non-empty revs"),
    }
}

// ---------------------------------------------------------------------------
// Ancestry lookup (for replication — provides _revisions data)
// ---------------------------------------------------------------------------

/// Find the revision ancestry chain for a specific revision in the tree.
///
/// Returns hashes from leaf to root: `[target_hash, parent_hash, grandparent_hash, ...]`
/// or `None` if the revision is not found in the tree.
pub fn find_rev_ancestry(
    tree: &RevTree,
    target_pos: u64,
    target_hash: &str,
) -> Option<Vec<String>> {
    for path in tree {
        if let Some(chain) = find_chain_in_node(&path.tree, path.pos, target_pos, target_hash) {
            return Some(chain);
        }
    }
    None
}

fn find_chain_in_node(
    node: &RevNode,
    current_pos: u64,
    target_pos: u64,
    target_hash: &str,
) -> Option<Vec<String>> {
    if current_pos == target_pos && node.hash == target_hash {
        return Some(vec![node.hash.clone()]);
    }

    for child in &node.children {
        if let Some(mut chain) = find_chain_in_node(child, current_pos + 1, target_pos, target_hash)
        {
            chain.push(node.hash.clone());
            return Some(chain);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(hash: &str) -> RevNode {
        RevNode {
            hash: hash.into(),
            status: RevStatus::Available,
            opts: NodeOpts::default(),
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

    #[test]
    fn collect_leaves_simple_chain() {
        // 1-a -> 2-b -> 3-c
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![node("b", vec![leaf("c")])]),
        }];
        let leaves = collect_leaves(&tree);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].pos, 3);
        assert_eq!(leaves[0].hash, "c");
    }

    #[test]
    fn collect_leaves_with_conflict() {
        // 1-a -> 2-b (branch 1)
        //     -> 2-c (branch 2)
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b"), leaf("c")]),
        }];
        let leaves = collect_leaves(&tree);
        assert_eq!(leaves.len(), 2);
        // Sorted by hash desc: c before b
        assert_eq!(leaves[0].hash, "c");
        assert_eq!(leaves[1].hash, "b");
    }

    #[test]
    fn rev_exists_finds_nodes() {
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![leaf("b")]),
        }];
        assert!(rev_exists(&tree, 1, "a"));
        assert!(rev_exists(&tree, 2, "b"));
        assert!(!rev_exists(&tree, 1, "z"));
        assert!(!rev_exists(&tree, 3, "a"));
    }

    #[test]
    fn root_to_leaf_paths() {
        // 1-a -> 2-b -> 3-c
        //            -> 3-d
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![node("b", vec![leaf("c"), leaf("d")])]),
        }];
        let paths = root_to_leaf(&tree);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].0, 1); // root pos
        assert_eq!(paths[0].1.len(), 3); // a, b, c
        assert_eq!(paths[1].1.len(), 3); // a, b, d
    }

    #[test]
    fn find_rev_ancestry_linear_chain() {
        // 1-a -> 2-b -> 3-c
        let tree = vec![RevPath {
            pos: 1,
            tree: node("a", vec![node("b", vec![leaf("c")])]),
        }];
        let ancestry = find_rev_ancestry(&tree, 3, "c").unwrap();
        assert_eq!(ancestry, vec!["c", "b", "a"]);

        let ancestry = find_rev_ancestry(&tree, 2, "b").unwrap();
        assert_eq!(ancestry, vec!["b", "a"]);

        let ancestry = find_rev_ancestry(&tree, 1, "a").unwrap();
        assert_eq!(ancestry, vec!["a"]);

        assert!(find_rev_ancestry(&tree, 3, "z").is_none());
    }

    #[test]
    fn build_path_from_revs_works() {
        // revs: ["c", "b", "a"] (leaf first), pos: 3
        let path = build_path_from_revs(
            3,
            &["c".into(), "b".into(), "a".into()],
            NodeOpts::default(),
            RevStatus::Available,
        );
        assert_eq!(path.pos, 1);
        assert_eq!(path.tree.hash, "a");
        assert_eq!(path.tree.status, RevStatus::Missing);
        assert_eq!(path.tree.children.len(), 1);
        assert_eq!(path.tree.children[0].hash, "b");
        assert_eq!(path.tree.children[0].children[0].hash, "c");
        assert_eq!(
            path.tree.children[0].children[0].status,
            RevStatus::Available
        );
    }
}
