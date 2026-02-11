# Revision Trees

CouchDB's multi-version concurrency control (MVCC) model stores the entire
revision history of a document as a tree. Every edit creates a new leaf node.
Concurrent edits from different replicas create branches (conflicts). A
deterministic algorithm picks the "winning" revision so that every replica
independently converges on the same answer.

RouchDB implements this model in `rouchdb-core`, split across two files:

- `rev_tree.rs` -- data structures and traversal helpers
- `merge.rs` -- merging incoming revisions, winning-rev selection, conflict
  collection, and stemming

## Data Structures

### `RevNode`

A single node in the revision tree.

```rust
pub struct RevNode {
    pub hash: String,          // the hash portion of "pos-hash"
    pub status: RevStatus,     // Available | Missing
    pub opts: NodeOpts,        // metadata flags (e.g. deleted)
    pub children: Vec<RevNode>,// child revisions
}
```

- `hash` -- the hex MD5 digest that, together with `pos`, forms the full
  revision ID (e.g., `"3-a1b2c3..."`).
- `status` -- `Available` means the full document body for this revision is
  still stored; `Missing` means only the hash remains (after compaction or
  stemming).
- `opts.deleted` -- whether this revision marks a deletion (`_deleted: true`).
- `children` -- zero children means this is a leaf; two or more children means
  a conflict branch point.

### `RevPath`

A rooted sub-tree within the revision forest.

```rust
pub struct RevPath {
    pub pos: u64,        // generation number of the root node
    pub tree: RevNode,   // the root node of this sub-tree
}
```

`pos` is the generation number (the integer before the dash in `"3-abc"`). If
a tree has been stemmed and its earliest stored revision is generation 5, then
`pos = 5`.

### `RevTree`

```rust
pub type RevTree = Vec<RevPath>;
```

A document's full revision history. Most documents have a single `RevPath`
entry. Multiple entries appear when stemming creates disjoint sub-trees that
are later reconnected during replication.

### `LeafInfo`

```rust
pub struct LeafInfo {
    pub pos: u64,
    pub hash: String,
    pub deleted: bool,
    pub status: RevStatus,
}
```

A lightweight snapshot of a leaf node's identity and state, used by
`collect_leaves` and the winning-rev algorithm.

## Tree Structure: ASCII Examples

### Linear History (No Conflicts)

```
  1-aaa --> 2-bbb --> 3-ccc
```

Represented as:

```
RevTree = [
    RevPath {
        pos: 1,
        tree: RevNode("aaa", children: [
            RevNode("bbb", children: [
                RevNode("ccc", children: [])   // <-- leaf
            ])
        ])
    }
]
```

One root at position 1, one leaf at position 3.

### Conflict (Two Branches)

Two replicas independently edit generation 1, producing two conflicting
generation-2 revisions:

```
            +-- 2-bbb   (branch A)
            |
  1-aaa ----+
            |
            +-- 2-ccc   (branch B)
```

```
RevTree = [
    RevPath {
        pos: 1,
        tree: RevNode("aaa", children: [
            RevNode("bbb", children: []),   // leaf A
            RevNode("ccc", children: []),   // leaf B
        ])
    }
]
```

Both `2-bbb` and `2-ccc` are leaves. The one with the lexicographically
greater hash (`"ccc" > "bbb"`) wins.

### Divergent History After Stemming

After aggressive stemming, a document might have two disjoint sub-trees:

```
  3-ddd --> 4-eee --> 5-fff       (sub-tree A)

  3-ggg --> 4-hhh                 (sub-tree B, re-introduced by replication)
```

```
RevTree = [
    RevPath { pos: 3, tree: RevNode("ddd", ...) },
    RevPath { pos: 3, tree: RevNode("ggg", ...) },
]
```

Two roots, each with their own `pos`. This is normal and handled correctly by
all traversal and merge operations.

## Traversal Helpers

### `traverse_rev_tree`

Depth-first traversal of every node in the forest. The callback receives
`(absolute_pos, &RevNode, root_pos)` so it always knows each node's full
generation number.

### `collect_leaves`

Returns all leaf nodes (nodes with no children), sorted by the winning-rev
ordering:

1. Non-deleted before deleted
2. Higher generation first
3. Lexicographically greater hash first

This means `collect_leaves(&tree)[0]` is always the winner.

### `root_to_leaf`

Decomposes the tree into every root-to-leaf path. Used during replication to
provide `_revisions` ancestry data.

### `rev_exists`

Checks whether a specific `(pos, hash)` pair exists anywhere in the tree.
Used by `revs_diff` to determine which revisions the target is missing.

### `find_rev_ancestry`

Given a target `(pos, hash)`, walks the tree and returns the chain of hashes
from the target back to the root: `[target_hash, parent_hash, grandparent_hash, ...]`.
This is the data that populates the `_revisions` field in `bulk_get` responses.

### `build_path_from_revs`

Constructs a single-path `RevPath` from an array of revision hashes. The
input is newest-first (leaf-first): `[newest_hash, ..., oldest_hash]`. The leaf gets
`RevStatus::Available`; all ancestors get `RevStatus::Missing`. This is how
incoming revisions from replication are turned into a structure that can be
merged.

## The Merge Algorithm

When a new revision arrives (either from a local write or from replication),
it must be merged into the existing tree. The entry point is `merge_tree`:

```rust
pub fn merge_tree(
    tree: &RevTree,
    new_path: &RevPath,
    rev_limit: u64,
) -> (RevTree, MergeResult)
```

`MergeResult` is one of:

| Variant | Meaning |
|---|---|
| `NewLeaf` | The new path extended an existing branch (normal edit) |
| `NewBranch` | The new path created a new branch (conflict) |
| `InternalNode` | The new path's leaf already existed (duplicate / no-op) |

### Step-by-step walkthrough

**1. Try each existing root.** For each `RevPath` in the tree, call
`try_merge_path` to see if the new path overlaps with it.

**2. Find the overlap.** `find_overlap` flattens the new path into a linear
chain of `(pos, hash)` pairs, then searches the existing tree for any matching
node. If found, it records the navigation path (a `Vec<usize>` of child
indices) and the remaining new nodes that need to be grafted.

```
Existing:     1-aaa --> 2-bbb
New path:     2-bbb --> 3-ccc

Overlap at:   2-bbb
Remainder:    [RevNode("ccc")]
```

**3. Graft the remainder.** `graft_nodes` navigates to the overlap node and
appends the remainder as children.

```
After graft:  1-aaa --> 2-bbb --> 3-ccc
```

If the overlap node already had a different child, the new node becomes an
additional child -- creating a conflict branch:

```
Before:       1-aaa --> 2-bbb
New path:     1-aaa --> 2-ccc

After:        1-aaa --+--> 2-bbb
                      |
                      +--> 2-ccc    (new branch = conflict)
```

**4. No overlap: new root.** If no existing root shares any node with the new
path, the new path is pushed onto the `RevTree` as a separate root. This
happens when stemmed trees diverge during offline replication.

**5. Stemming.** After the merge, `stem` is called with the configured
`rev_limit` (default 1000). Stemming walks from the root toward the leaves
and removes nodes once the tree exceeds the depth limit.

### Example: Full Merge Sequence

Starting state (empty tree):

```
[]
```

Write `doc1` -- local write generates `1-a1b2`:

```
merge([], RevPath{pos:1, tree: RevNode("a1b2")})
  -> [RevPath{pos:1, tree: RevNode("a1b2")}]
  -> MergeResult::NewBranch   (first revision always creates a new root)
```

Update `doc1` with `_rev: "1-a1b2"` -- generates `2-c3d4`:

```
merge(tree, RevPath{pos:1, tree: RevNode("a1b2" -> "c3d4")})
  -> overlap at 1-a1b2, graft "c3d4" as child
  -> 1-a1b2 --> 2-c3d4
  -> MergeResult::NewLeaf
```

Meanwhile replica B writes `doc1` independently, producing `2-e5f6`.
Replication delivers it:

```
merge(tree, RevPath{pos:1, tree: RevNode("a1b2" -> "e5f6")})
  -> overlap at 1-a1b2, graft "e5f6" as second child
  -> 1-a1b2 --+--> 2-c3d4
              |
              +--> 2-e5f6
  -> MergeResult::NewBranch
```

## Winning Revision Algorithm

```rust
pub fn winning_rev(tree: &RevTree) -> Option<Revision>
```

The algorithm is completely deterministic -- every replica arrives at the same
answer without communication:

1. **Non-deleted beats deleted.** A leaf that is not marked `_deleted: true`
   always wins over a leaf that is deleted, regardless of generation or hash.

2. **Higher generation wins.** Among leaves with the same deleted status, the
   one with the higher `pos` (generation number) wins.

3. **Lexicographic hash breaks ties.** If two leaves have the same `pos` and
   same deleted status, the one with the lexicographically greater hash wins.

Implementation-wise, `winning_rev` calls `collect_leaves` (which sorts by
exactly this ordering) and returns the first element.

### Examples

```
1-aaa --+--> 2-bbb
        |
        +--> 2-ccc

Winner: 2-ccc   (same pos, "ccc" > "bbb")
```

```
1-aaa --+--> 2-bbb --> 3-ddd
        |
        +--> 2-ccc

Winner: 3-ddd   (pos 3 > pos 2)
```

```
1-aaa --+--> 2-bbb           (not deleted)
        |
        +--> 2-zzz [deleted]

Winner: 2-bbb   (non-deleted beats deleted, even though "zzz" > "bbb")
```

## Collecting Conflicts

```rust
pub fn collect_conflicts(tree: &RevTree) -> Vec<Revision>
```

Returns all non-winning, non-deleted leaf revisions. These are the revisions
that a user or application must resolve. The function calls `collect_leaves`,
skips the first entry (the winner), and filters out deleted leaves.

## Stemming (Pruning)

```rust
pub fn stem(tree: &mut RevTree, depth: u64) -> Vec<String>
```

Over time, revision trees grow indefinitely. Stemming prunes ancestor nodes
that are more than `depth` generations away from any leaf.

The algorithm:

1. For each `RevPath`, compute the maximum depth from root to any leaf.
2. If the depth exceeds the limit, remove nodes from the root until the
   deepest path has at most `depth` nodes.
3. When the root is removed, the root's single child becomes the new root
   and `pos` is incremented by 1.
4. Stemming stops at branch points -- you cannot remove a node that has
   multiple children, because that would disconnect branches.
5. Any `RevPath` that becomes empty is removed from the tree.

```
Before (depth limit = 3):

  1-aaa --> 2-bbb --> 3-ccc --> 4-ddd --> 5-eee

After:

  3-ccc --> 4-ddd --> 5-eee
  (pos adjusted from 1 to 3; "aaa" and "bbb" are returned as stemmed)
```

The default `rev_limit` in the redb adapter is 1000, which matches CouchDB's
default. PouchDB defaults to 1000 as well.

## Revision Hash Generation

When `new_edits=true` (normal local write), the adapter generates revision
hashes deterministically:

```rust
fn generate_rev_hash(
    doc_data: &serde_json::Value,
    deleted: bool,
    prev_rev: Option<&str>,
) -> String
```

The hash is MD5 over: `previous_rev_string + ("1" if deleted else "0") + JSON-serialized body`.

This means the same edit on the same predecessor always produces the same
revision ID, which is important for idempotency.

When `new_edits=false` (replication mode), the adapter accepts the revision ID
as-is from the source and merges it into the tree without generating a new hash.
