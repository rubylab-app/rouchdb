//! Revision tree micro-benchmarks: `merge_tree`, `stem`, `winning_rev` and
//! `collect_conflicts` on deep (linear history) and wide (many conflicts)
//! trees.
//!
//! Run: cargo bench -p rouchdb-bench --bench revtree

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use rouchdb_core::merge::{collect_conflicts, is_deleted, merge_tree, stem, winning_rev};
use rouchdb_core::rev_tree::{NodeOpts, RevNode, RevPath, RevStatus, RevTree};

/// CouchDB's default `_revs_limit`.
const REV_LIMIT: u64 = 1000;

fn hash(n: u64) -> String {
    format!("{n:032x}")
}

fn node(hash: String, status: RevStatus, children: Vec<RevNode>) -> RevNode {
    RevNode {
        hash,
        status,
        opts: NodeOpts::default(),
        children,
    }
}

/// A linear path of generations `first..=last`: the leaf is available and
/// every ancestor is missing, like a path built from `_revisions`.
fn chain(first: u64, last: u64) -> RevPath {
    let mut tree = node(hash(last), RevStatus::Available, Vec::new());
    for generation in (first..last).rev() {
        tree = node(hash(generation), RevStatus::Missing, vec![tree]);
    }
    RevPath { pos: first, tree }
}

/// A root with `leaves` conflicting generation-2 children (every 10th one
/// deleted).
fn wide(leaves: u64) -> RevTree {
    let children = (0..leaves)
        .map(|i| RevNode {
            hash: hash(1_000_000 + i),
            status: RevStatus::Available,
            opts: NodeOpts {
                deleted: i % 10 == 0,
            },
            children: Vec::new(),
        })
        .collect();
    vec![RevPath {
        pos: 1,
        tree: node(hash(0), RevStatus::Missing, children),
    }]
}

fn bench_merge(c: &mut Criterion) {
    let mut g = c.benchmark_group("merge_tree");

    // A normal edit: `[parent, new leaf]` on top of a linear history.
    for depth in [10u64, 100, 1_000] {
        let tree: RevTree = vec![chain(1, depth)];
        let edit = chain(depth, depth + 1);
        g.bench_with_input(BenchmarkId::new("linear_edit", depth), &depth, |b, _| {
            b.iter(|| merge_tree(black_box(&tree), black_box(&edit), REV_LIMIT))
        });
    }

    // Replication into a document already stemmed to 1000 revisions
    // (generations 1001..=2000).
    let stemmed: RevTree = vec![chain(1_001, 2_000)];
    let incoming = [
        ("replicate_overlap_900", chain(1_101, 2_100)),
        ("replicate_disjoint_1000", chain(2_001, 3_000)),
        ("replicate_duplicate", chain(1_001, 2_000)),
    ];
    for (name, path) in &incoming {
        g.bench_function(*name, |b| {
            b.iter(|| merge_tree(black_box(&stemmed), black_box(path), REV_LIMIT))
        });
    }

    let empty: RevTree = Vec::new();
    let full = chain(1, 1_000);
    g.bench_function("replicate_new_doc_1000", |b| {
        b.iter(|| merge_tree(black_box(&empty), black_box(&full), REV_LIMIT))
    });

    // A new conflicting branch on a document that already has 100 leaves.
    let wide_tree = wide(100);
    let branch = RevPath {
        pos: 1,
        tree: node(
            hash(0),
            RevStatus::Missing,
            vec![node(hash(999_999_999), RevStatus::Available, Vec::new())],
        ),
    };
    g.bench_function("new_conflict_on_100_leaves", |b| {
        b.iter(|| merge_tree(black_box(&wide_tree), black_box(&branch), REV_LIMIT))
    });
    g.finish();
}

fn bench_stem(c: &mut Criterion) {
    let mut g = c.benchmark_group("stem");
    for len in [1_001u64, 2_000] {
        let tree: RevTree = vec![chain(1, len)];
        g.bench_with_input(BenchmarkId::new("linear_to_1000", len), &len, |b, _| {
            b.iter_batched(
                || tree.clone(),
                |mut t| {
                    let removed = stem(&mut t, REV_LIMIT);
                    (t, removed)
                },
                BatchSize::SmallInput,
            )
        });
    }
    g.finish();
}

fn bench_winner(c: &mut Criterion) {
    let mut g = c.benchmark_group("winner");
    let trees = [
        ("deep_1000", vec![chain(1, 1_000)]),
        ("wide_100", wide(100)),
        ("wide_1000", wide(1_000)),
    ];
    for (name, tree) in &trees {
        g.bench_function(BenchmarkId::new("winning_rev", name), |b| {
            b.iter(|| winning_rev(black_box(tree)))
        });
        g.bench_function(BenchmarkId::new("collect_conflicts", name), |b| {
            b.iter(|| collect_conflicts(black_box(tree)))
        });
        // What a `get` with `conflicts: true` computes per document.
        g.bench_function(BenchmarkId::new("winner_deleted_conflicts", name), |b| {
            b.iter(|| {
                let t = black_box(tree);
                (winning_rev(t), is_deleted(t), collect_conflicts(t))
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench_merge, bench_stem, bench_winner);
criterion_main!(benches);
