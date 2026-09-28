# Testing

RouchDB has **unit tests** inside each crate, **integration tests** of the `Database` API that run on the local backends (memory and redb), and **CouchDB tests** that need a running CouchDB instance.

## Unit Tests

Unit tests are defined as `#[cfg(test)]` modules inside each crate's source files. They cover internal logic without any external services.

### Running All Unit Tests

```bash
cargo test
```

This runs every test that does not need CouchDB: the unit tests of the 12 workspace crates and the integration tests in `crates/rouchdb/tests/` that use the memory and redb backends.

### Running Tests for a Single Crate

```bash
cargo test -p rouchdb-core
cargo test -p rouchdb-adapter-memory
cargo test -p rouchdb-query
```

### Running a Specific Test

```bash
cargo test -p rouchdb-core winning_rev_simple
```

## Integration Tests

Integration tests live in `crates/rouchdb/tests/`. Most of them exercise the `Database` API on the local backends and run with plain `cargo test`:

- `adapter_conformance.rs` runs every scenario on both the memory and the redb adapter (the `conformance!` macro);
- contract suites such as `plugin_contract.rs`, `partition.rs` and `error_conditions.rs` loop over both backends with the `backends()` helper of `crates/rouchdb/tests/backends/mod.rs`.

The tests that verify RouchDB against a real CouchDB server (protocol compliance, replication, parity of results) are spread over the same files (`http_crud.rs`, `replication.rs`, `couchdb_query_parity.rs`, `data_diversity.rs`, etc.) and are marked `#[ignore]`.

### Prerequisites

Start CouchDB via Docker Compose:

```bash
docker compose up -d
```

Wait for the health check to pass (the service should report `healthy`):

```bash
docker compose ps
```

The default connection URL is `http://admin:password@localhost:15984`.

### Running Integration Tests

The CouchDB tests are marked `#[ignore]` so they are skipped during `cargo test`. Run them (every crate, one test at a time) with:

```bash
bash scripts/test-couchdb.sh
```

The script runs `cargo test --workspace --no-fail-fast -- --ignored --test-threads=1` and skips the tests marked `#[ignore = "blocked on …"]` (see below). Extra arguments go to `cargo test`, e.g. `bash scripts/test-couchdb.sh -p rouchdb`.

To run a single integration test by name:

```bash
cargo test -p rouchdb --test http_crud http_put_and_get -- --ignored
```

### Custom CouchDB URL

To point tests at a different CouchDB instance, set the `COUCHDB_URL` environment variable:

```bash
COUCHDB_URL="http://user:pass@myhost:5984" bash scripts/test-couchdb.sh
```

### Tests Blocked on a Known Bug

A test that pins down a known, not yet fixed library bug is marked with the finding it is waiting for, instead of weakening its assertions or asserting the wrong behavior:

```rust
#[tokio::test]
#[ignore = "blocked on Q-API-1: partition bounds drop edge ids"]
async fn partition_all_docs_edge_ids() { /* ... */ }
```

`cargo test` skips it like any ignored test, and `scripts/test-couchdb.sh` (and therefore CI) skips it too, whether or not it needs CouchDB. Run one explicitly with `cargo test -p rouchdb --test partition partition_all_docs_edge_ids -- --ignored`, and remove the marker in the PR that fixes the bug.

## Writing New Unit Tests

Unit tests go in a `#[cfg(test)]` module at the bottom of the source file they are testing.

### Synchronous Tests (pure logic)

For functions that do not involve async I/O, use standard `#[test]`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_pure_logic_test() {
        let tree = build_some_rev_tree();
        let winner = winning_rev(&tree).unwrap();
        assert_eq!(winner.pos, 3);
        assert_eq!(winner.hash, "abc");
    }
}
```

This pattern is used extensively in `rouchdb-core` for revision tree operations, merge algorithms, and collation ordering.

### Async Tests (adapter operations)

For tests that exercise adapter methods, use `#[tokio::test]`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rouchdb_core::document::{AllDocsOptions, BulkDocsOptions, GetOptions};

    async fn new_db() -> MemoryAdapter {
        MemoryAdapter::new("test")
    }

    #[tokio::test]
    async fn put_and_get_document() {
        let db = new_db().await;

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"name": "Alice"}),
            attachments: HashMap::new(),
        };

        let results = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(results[0].ok);

        let fetched = db.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(fetched.data["name"], "Alice");
    }
}
```

### Guidelines for Unit Tests

- Place tests in the same file as the code they exercise.
- Use a helper function (e.g., `new_db()`) to create a fresh adapter instance per test.
- Test both the success path and error conditions.
- Keep tests focused -- one logical assertion per test function when practical.

## Writing New Integration Tests

Integration tests go in `crates/rouchdb/tests/` as separate test files. They test the high-level `Database` API.

### Tests on the Local Backends

Run a `Database` test on both local backends unless it is about one of them:

```rust
mod backends;

use backends::backends;

#[tokio::test]
async fn my_local_test() {
    for b in backends("test") {
        let r = b.db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let doc = b.db.get("doc1").await.unwrap();
        assert_eq!(doc.rev.unwrap().to_string(), r.rev.unwrap(), "{}", b.name);
        assert_eq!(doc.data, serde_json::json!({"v": 1}), "{}", b.name);
    }
}
```

A scenario that every adapter must pass belongs in `adapter_conformance.rs`.

### Structure of a CouchDB Test

Every CouchDB integration test follows this pattern:

```rust
#[tokio::test]
#[ignore]
async fn my_couchdb_test() {
    // 1. Create a fresh database with a unique name
    let url = fresh_remote_db("my_test_prefix").await;
    let db = Database::http(&url);

    // 2. Perform operations
    let result = db.put("doc1", serde_json::json!({"key": "value"})).await.unwrap();
    assert!(result.ok);

    // 3. Verify results
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["key"], "value");

    // 4. Clean up the database
    delete_remote_db(&url).await;
}
```

Key points:

- **Always add `#[ignore]`** to a test that needs CouchDB, so it does not run in `cargo test`.
- **Always use `fresh_remote_db()`** to get a uniquely-named database. This prevents test interference.
- **Always call `delete_remote_db()`** at the end to clean up.
- The `fresh_remote_db()` helper creates the database via the CouchDB REST API and returns its full URL.

### When to Write an Integration Test

Add an integration test when you need to verify:

- HTTP adapter correctness against a real CouchDB server
- Replication between a local adapter and CouchDB
- Protocol-level compatibility (e.g., `_revs_diff`, `_bulk_get` responses)
- Edge cases that depend on CouchDB-specific behavior

## Test Patterns

### Memory Adapter for Fast Tests

The `MemoryAdapter` is the primary tool for fast, isolated unit tests. It implements the full `Adapter` trait in memory with no I/O, making tests instant and deterministic. The redb adapter is almost as fast on a `tempfile` directory, so `Database` tests should run on both (see above).

Use `MemoryAdapter` when testing:

- Replication protocol logic (by replicating between two memory adapters)
- Changes feed behavior
- Query/selector matching
- Any feature that works at the `Adapter` trait level

Example from the replication crate:

```rust
let source = MemoryAdapter::new("source");
let target = MemoryAdapter::new("target");

// ... write docs to source ...

replicate(&source, &target, ReplicationOptions::default()).await.unwrap();

// ... verify docs appear in target ...
```

### Real CouchDB for Protocol Compliance

Integration tests with a real CouchDB instance catch issues that in-memory tests cannot:

- JSON serialization/deserialization mismatches
- HTTP header requirements
- CouchDB-specific revision handling quirks
- Sequence format differences between CouchDB versions
- Attachment encoding edge cases

### Helper Functions in Integration Tests

The CouchDB test files share three common helpers from `crates/rouchdb/tests/common/mod.rs`:

- `couchdb_url()` -- Returns the CouchDB base URL, respecting the `COUCHDB_URL` environment variable.
- `fresh_remote_db(prefix)` -- Creates a new CouchDB database with a UUID-based name and returns its URL.
- `delete_remote_db(url)` -- Deletes a CouchDB database by URL.

## Assertions

A test must be able to fail. In particular:

- `put`, `update`, `remove` and `post` return a failed write as an error (`Err(RouchError::Conflict)`, `Err(RouchError::NotFound(_))`, `Err(RouchError::Forbidden(_))`, ...), never as `Ok` with `ok: false`. `bulk_docs` reports failures per document (`DocResult { ok: false, error: Some("conflict"), .. }`), so check each result's `ok` or `error`.
- Check the exact error with `matches!(result, Err(RouchError::Conflict))`, not `is_err()`: an unrelated failure passes `is_err()` too.
- Compare exact values: the list of ids in order, the whole body, the revision. A count passes with the wrong documents, and `for row in &rows { assert!(..) }` passes when there are no rows.
- Check the preconditions a test relies on (for example, which revision of a conflict wins) and that a rejected write left the database unchanged (same `update_seq`, same revision).
- Do not accept every outcome (`assert!(r.is_ok() || r.is_err())`, `match` arms that all do nothing). Assert the behavior CouchDB has, and verify it against a real CouchDB when in doubt.

## Benchmarks

Criterion benchmarks live in the `rouchdb-bench` crate:

- `benches/core_ops.rs`: `bulk_docs`, `get`, `all_docs`, `changes`, Mango `find` with and without an index, and replication, on the memory and redb backends.
- `benches/revtree.rs`: `merge_tree`, `stem`, `winning_rev` and `collect_conflicts` on deep and wide revision trees.

```bash
cargo bench -p rouchdb-bench                   # everything
cargo bench -p rouchdb-bench -- find/redb      # filter by benchmark name
ROUCHDB_BENCH_N=100000 cargo bench -p rouchdb-bench --bench core_ops
cargo bench -p rouchdb-bench -- --save-baseline main   # then compare with --baseline main
```

Results depend heavily on the machine; compare runs on the same machine only.

## Continuous Integration

GitHub Actions (`.github/workflows/ci.yml`) runs these jobs on every pull request:

| Job | What it runs |
|-----|--------------|
| Check & Lint | `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, and the TLS feature combinations |
| Tests | `cargo test --workspace` |
| CouchDB integration tests | `scripts/test-couchdb.sh` against a `couchdb:3` service container |
| Benchmarks (build only) | `cargo bench --no-run` |
| MSRV (1.88) | `cargo check --all-targets --all-features` on Rust 1.88 |
| Clippy on stable/beta | Non-blocking early warning about lints from newer toolchains |

The blocking jobs use the toolchain pinned in `rust-toolchain.toml`. To move to a newer Rust, bump it there and fix any new lints in the same PR. The benchmarks can be run on a GitHub runner from the Actions tab (the manual "Benchmarks" workflow); shared runners are noisy, so use those numbers for trends only.
