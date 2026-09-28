//! Shared fixtures for the RouchDB benchmarks in `benches/`.
//!
//! ```text
//! cargo bench -p rouchdb-bench                       # everything
//! cargo bench -p rouchdb-bench --bench core_ops      # Database API only
//! cargo bench -p rouchdb-bench --bench revtree       # rev tree algorithms
//! cargo bench -p rouchdb-bench -- all_docs           # filter by name
//! ROUCHDB_BENCH_N=100000 cargo bench -p rouchdb-bench --bench core_ops
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use rouchdb::{Adapter, BulkDocsOptions, Database, Document, MemoryAdapter, RedbAdapter};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// Number of documents in the read/query datasets (`ROUCHDB_BENCH_N`,
/// default 10 000).
pub fn dataset_n() -> usize {
    std::env::var("ROUCHDB_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

/// Single-threaded Tokio runtime shared by a benchmark group.
pub fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// Storage backend under test.
#[derive(Clone, Copy, Debug)]
pub enum Backend {
    Memory,
    Redb,
}

impl Backend {
    pub const ALL: [Backend; 2] = [Backend::Memory, Backend::Redb];

    pub fn name(self) -> &'static str {
        match self {
            Backend::Memory => "memory",
            Backend::Redb => "redb",
        }
    }
}

/// A database plus whatever keeps its storage alive.
///
/// Field order matters: the handles drop before the temp dir is deleted.
pub struct Fixture {
    pub db: Database,
    adapter: Arc<dyn Adapter>,
    _dir: Option<TempDir>,
}

impl Fixture {
    /// An empty database on `backend` (redb files live in a temp dir).
    pub fn new(backend: Backend) -> Self {
        let (adapter, dir): (Arc<dyn Adapter>, Option<TempDir>) = match backend {
            Backend::Memory => (Arc::new(MemoryAdapter::new("bench")), None),
            Backend::Redb => {
                let dir = tempfile::tempdir().expect("tempdir");
                let adapter =
                    RedbAdapter::open(dir.path().join("bench.redb"), "bench").expect("open redb");
                (Arc::new(adapter), Some(dir))
            }
        };
        Fixture {
            db: Database::from_adapter(Arc::clone(&adapter)),
            adapter,
            _dir: dir,
        }
    }

    /// A database on `backend` holding `n` documents (see [`body`]).
    pub fn populated(rt: &Runtime, backend: Backend, n: usize) -> Self {
        let fx = Self::new(backend);
        rt.block_on(populate(&fx.db, 0, n));
        fx
    }

    /// Another `Database` over the same storage, with its own (empty) Mango
    /// index cache.
    pub fn another_handle(&self) -> Database {
        Database::from_adapter(Arc::clone(&self.adapter))
    }
}

pub fn doc_id(i: usize) -> String {
    format!("doc{i:08}")
}

/// A ~250 byte document with a low-cardinality `group`, a numeric `age` and
/// a nested `address`.
pub fn body(i: usize) -> Value {
    json!({
        "type": "user",
        "group": i % 100,
        "name": format!("user {i}"),
        "age": (i * 7) % 90,
        "tags": ["a", "b", "c"],
        "address": { "city": format!("city{}", i % 50), "zip": format!("{:05}", i % 99_999) }
    })
}

pub fn new_doc(i: usize) -> Document {
    Document {
        id: doc_id(i),
        rev: None,
        deleted: false,
        data: body(i),
        attachments: HashMap::new(),
    }
}

/// Writes docs `from..to` in batches of 1 000 and asserts every write
/// succeeded.
pub async fn populate(db: &Database, from: usize, to: usize) {
    let mut start = from;
    while start < to {
        let end = (start + 1_000).min(to);
        let docs: Vec<Document> = (start..end).map(new_doc).collect();
        let results = db
            .bulk_docs(docs, BulkDocsOptions::new())
            .await
            .expect("populate bulk_docs");
        assert!(results.iter().all(|r| r.ok), "populate: a write failed");
        start = end;
    }
}
