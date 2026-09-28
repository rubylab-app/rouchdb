//! End-to-end benchmarks through the public `rouchdb::Database` API, on the
//! memory and redb backends: bulk writes, `get`, `all_docs`, `changes`,
//! Mango `find` (with and without an index) and replication.
//!
//! Run: cargo bench -p rouchdb-bench --bench core_ops [-- <filter>]
//! Dataset size: ROUCHDB_BENCH_N (default 10 000 documents).

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rouchdb::{
    AllDocsOptions, BulkDocsOptions, ChangesOptions, Document, FindOptions, IndexDefinition,
    ReplicationOptions, Seq, SortField,
};
use rouchdb_bench::{Backend, Fixture, dataset_n, doc_id, new_doc, runtime};
use serde_json::json;

fn bench_writes(c: &mut Criterion) {
    let rt = runtime();
    let mut g = c.benchmark_group("write");
    g.sample_size(10);

    for n in [100usize, 1_000, 10_000] {
        g.throughput(Throughput::Elements(n as u64));
        for backend in Backend::ALL {
            let id = BenchmarkId::new(format!("bulk_docs_insert/{}", backend.name()), n);
            g.bench_function(id, |b| {
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            // A fresh database per iteration, created untimed.
                            let fx = Fixture::new(backend);
                            let docs: Vec<Document> = (0..n).map(new_doc).collect();
                            let start = Instant::now();
                            let results = fx
                                .db
                                .bulk_docs(docs, BulkDocsOptions::new())
                                .await
                                .expect("bulk_docs");
                            total += start.elapsed();
                            assert!(results.iter().all(|r| r.ok), "a write failed");
                        }
                        total
                    })
                })
            });
        }
    }
    g.finish();
}

fn bench_reads(c: &mut Criterion) {
    let rt = runtime();
    let n = dataset_n();
    let mid = n / 2;

    for backend in Backend::ALL {
        let fx = Fixture::populated(&rt, backend, n);
        let mut g = c.benchmark_group(format!("read/{}", backend.name()));

        let mut i = 0usize;
        g.bench_function(BenchmarkId::new("get", n), |b| {
            b.iter(|| {
                i = (i + 7_919) % n;
                rt.block_on(fx.db.get(&doc_id(i))).expect("get")
            })
        });

        let all_docs_cases: Vec<(&str, AllDocsOptions, usize)> = vec![
            (
                "limit10",
                AllDocsOptions {
                    limit: Some(10),
                    ..AllDocsOptions::new()
                },
                10,
            ),
            (
                "key_include_docs",
                AllDocsOptions {
                    key: Some(doc_id(mid)),
                    include_docs: true,
                    ..AllDocsOptions::new()
                },
                1,
            ),
            (
                "keys10",
                AllDocsOptions {
                    keys: Some((mid..mid + 10).map(doc_id).collect()),
                    ..AllDocsOptions::new()
                },
                10,
            ),
            (
                "range100",
                AllDocsOptions {
                    start_key: Some(doc_id(mid)),
                    end_key: Some(doc_id(mid + 99)),
                    ..AllDocsOptions::new()
                },
                100,
            ),
            (
                "full_include_docs",
                AllDocsOptions {
                    include_docs: true,
                    ..AllDocsOptions::new()
                },
                n,
            ),
        ];
        for (name, opts, expected_rows) in all_docs_cases {
            let rows = rt
                .block_on(fx.db.all_docs(opts.clone()))
                .expect("all_docs")
                .rows
                .len();
            assert_eq!(rows, expected_rows, "all_docs/{name}");
            g.bench_function(BenchmarkId::new(format!("all_docs/{name}"), n), |b| {
                b.iter(|| rt.block_on(fx.db.all_docs(opts.clone())).expect("all_docs"))
            });
        }

        let update_seq = rt.block_on(fx.db.info()).expect("info").update_seq.as_num();
        let changes_cases: Vec<(&str, ChangesOptions, usize)> = vec![
            (
                "since0_limit100",
                ChangesOptions {
                    limit: Some(100),
                    ..Default::default()
                },
                100,
            ),
            (
                "tail_limit100",
                ChangesOptions {
                    since: Seq::Num(update_seq.saturating_sub(100)),
                    limit: Some(100),
                    ..Default::default()
                },
                100,
            ),
            (
                "since0_limit100_include_docs",
                ChangesOptions {
                    limit: Some(100),
                    include_docs: true,
                    ..Default::default()
                },
                100,
            ),
            ("since0_full", ChangesOptions::default(), n),
        ];
        for (name, opts, expected) in changes_cases {
            let events = rt
                .block_on(fx.db.changes(opts.clone()))
                .expect("changes")
                .results
                .len();
            assert_eq!(events, expected, "changes/{name}");
            g.bench_function(BenchmarkId::new(format!("changes/{name}"), n), |b| {
                b.iter(|| rt.block_on(fx.db.changes(opts.clone())).expect("changes"))
            });
        }
        g.finish();
    }
}

fn bench_find(c: &mut Criterion) {
    let rt = runtime();
    let n = dataset_n();

    for backend in Backend::ALL {
        let fx = Fixture::populated(&rt, backend, n);
        // Same storage, but only this handle knows about the index.
        let indexed = fx.another_handle();
        rt.block_on(indexed.create_index(IndexDefinition {
            name: "idx-group".into(),
            fields: vec![SortField::Simple("group".into())],
            ddoc: None,
        }))
        .expect("create_index");

        let mut g = c.benchmark_group(format!("find/{}", backend.name()));
        g.sample_size(20);
        let queries = [
            ("eq_group", json!({ "group": 42 }), true),
            (
                "range_group",
                json!({ "group": { "$gte": 10, "$lt": 12 } }),
                true,
            ),
            (
                "regex_name",
                json!({ "name": { "$regex": "^user 1" } }),
                false,
            ),
        ];
        for (name, selector, index_applies) in queries {
            let opts = FindOptions {
                selector,
                ..Default::default()
            };
            let scanned = rt.block_on(fx.db.find(opts.clone())).expect("find").docs;
            assert!(!scanned.is_empty(), "find/{name} matched nothing");
            g.bench_function(BenchmarkId::new(format!("{name}/no_index"), n), |b| {
                b.iter(|| rt.block_on(fx.db.find(opts.clone())).expect("find"))
            });
            if index_applies {
                let via_index = rt.block_on(indexed.find(opts.clone())).expect("find").docs;
                assert_eq!(
                    via_index.len(),
                    scanned.len(),
                    "find/{name}: index and full scan disagree"
                );
                g.bench_function(BenchmarkId::new(format!("{name}/with_index"), n), |b| {
                    b.iter(|| rt.block_on(indexed.find(opts.clone())).expect("find"))
                });
            }
        }
        g.finish();
    }
}

fn bench_replication(c: &mut Criterion) {
    let rt = runtime();
    let n = dataset_n();
    let source = Fixture::populated(&rt, Backend::Memory, n);

    let mut g = c.benchmark_group("replicate");
    g.sample_size(10);
    g.throughput(Throughput::Elements(n as u64));
    for target in Backend::ALL {
        let id = BenchmarkId::new(format!("memory_to_{}", target.name()), n);
        g.bench_function(id, |b| {
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let target = Fixture::new(target);
                        let opts = ReplicationOptions {
                            checkpoint: false,
                            ..Default::default()
                        };
                        let start = Instant::now();
                        let result = source
                            .db
                            .replicate_to_with_opts(&target.db, opts)
                            .await
                            .expect("replicate");
                        total += start.elapsed();
                        assert!(result.ok, "replication errors: {:?}", result.errors);
                        assert_eq!(result.docs_written, n as u64);
                        black_box(target);
                    }
                    total
                })
            })
        });
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets = bench_writes, bench_reads, bench_find, bench_replication
}
criterion_main!(benches);
