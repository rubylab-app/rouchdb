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

The tests that verify RouchDB against a real CouchDB server (protocol compliance, replication, parity of results) are spread over the same files (`http_crud.rs`, `replication.rs`, `couchdb_query_parity.rs`, `data_diversity.rs`, etc.) and are marked `#[ignore = "requires CouchDB"]`.

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

The CouchDB tests are marked `#[ignore = "requires CouchDB"]` so they are skipped during `cargo test`. Run them (every crate, in parallel) with:

```bash
bash scripts/test-couchdb.sh
```

The script runs `cargo test --workspace --tests --no-fail-fast -- --ignored --skip blocked_on_`, which leaves out the tests blocked on a known bug (see below). Extra arguments go to `cargo test`, e.g. `bash scripts/test-couchdb.sh -p rouchdb`. It fails if a test leaves one of its databases behind; `bash scripts/test-couchdb.sh --sweep` deletes every `rouchdb_test_*` database (leftovers of a killed run; do not run it while another suite uses the same server).

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

A test that pins down a known, not yet fixed library bug keeps its assertions. It is ignored with the finding it is waiting for, and its name contains `blocked_on_` (a `blocked_on_fxx_` prefix, or a `mod blocked_on_fxx`):

```rust
#[tokio::test]
#[ignore = "blocked on F03"]
async fn blocked_on_f03_inline_base64_attachment_decoding() { /* ... */ }
```

The name is what the scripts filter on, so no script has to parse the source:

- `scripts/test-couchdb.sh` (and therefore CI) skips them with `--skip blocked_on_`.
- `scripts/test-blocked.sh` runs only them and fails if any of them passes. CI runs it as a non-blocking step, so a fix that forgets to unblock its test shows up.

Run one explicitly with `cargo test -p rouchdb --test parity_core blocked_on_f03 -- --ignored`, and remove both the marker and the name prefix in the PR that fixes the bug. `#[ignore]` takes one of these two reasons only; CI rejects a bare `#[ignore]` or any other reason.

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
mod common;

use common::fresh_remote_db;

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn my_couchdb_test() {
    // 1. Create a fresh database with a unique name. `url` is a guard that
    //    deletes the database when it is dropped, even if the test fails.
    let url = fresh_remote_db("my_test_label").await;
    let db = Database::http(&url);

    // 2. Perform operations
    let result = db.put("doc1", serde_json::json!({"key": "value"})).await.unwrap();
    assert!(result.ok);

    // 3. Verify results
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["key"], "value");
}
```

Key points:

- **Always add `#[ignore = "requires CouchDB"]`** so the test does not run in `cargo test`, and so the CouchDB suite picks it up.
- **Always use `fresh_remote_db()`** (or `unique_remote_db()` when the code under test creates the database itself) to get a uniquely named database. This prevents test interference, so the suite can run in parallel.
- **No manual cleanup.** The returned `RemoteDb` guard deletes the database on drop, also when an assert fails. Set `ROUCHDB_KEEP_TEST_DBS=1` to keep the databases for debugging.
- **Do not hardcode the server or credentials.** Take them from `common::couchdb()` (`url`, `anonymous_url`, `user`, `password`), which parses `COUCHDB_URL` once.
- **Never log in as the admin with a wrong password.** CouchDB 3.4+ locks the account after a few failures and the rest of the suite then fails with 403. Use a random, nonexistent user name to test a failed login.

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

The integration tests share these helpers in `crates/rouchdb/tests/common/mod.rs` (the CLI tests include the same file):

- `couchdb()` -- The server under test, parsed once from the `COUCHDB_URL` environment variable: URL with and without credentials, user and password.
- `fresh_remote_db(label)` -- Creates a CouchDB database named `rouchdb_test_<label>_<uuid>` and returns a `RemoteDb` guard that derefs to its URL and deletes it when dropped.
- `unique_remote_db(label)` -- The same guard for a database that is not created up front.

Every test database starts with `rouchdb_test_`. If a run is killed before the guards run, remove the leftovers with `bash scripts/test-couchdb.sh --sweep`.

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

GitHub Actions (`.github/workflows/ci.yml`) runs these jobs on every pull request, each with a `timeout-minutes`:

| Job | What it runs |
|-----|--------------|
| Check & Lint | `cargo fmt --check`, every `#[ignore]` has a known reason, `cargo clippy --all-targets -D warnings`, and the TLS feature combinations |
| Tests | `cargo test --workspace --no-fail-fast` (including the README examples as doctests) |
| CouchDB integration tests | `scripts/test-couchdb.sh` against a `couchdb:3.5.1` service container (account lockout off), the non-blocking `scripts/test-blocked.sh` xfail check, and a check that no database was left behind |
| Benchmarks (build + smoke run) | `cargo bench --no-run`, then every benchmark once in criterion's `--test` mode on 1000 documents |
| MSRV (1.88) | `cargo check --all-targets --all-features` on Rust 1.88 |
| Clippy on stable/beta | Non-blocking early warning about lints from newer toolchains |

`minimal-versions.yml` resolves every direct dependency to the lowest version its `Cargo.toml` requirement allows (`cargo +nightly update -Z direct-minimal-versions`) and builds, when a manifest changes and weekly. When it fails, raise the requirement to the version the code actually needs, in every crate that declares it.

Three more workflows measure test quality. They never block a merge:

| Workflow | When | What |
|----------|------|------|
| `coverage.yml` | push to `main` | Line coverage (cargo-llvm-cov) of the unit tests, doctests and CouchDB suite: job summary plus an lcov artifact |
| `mutants.yml` | pull requests that change `crates/*/src` | `cargo mutants --in-diff` on the changed code (`--timeout 120`): mutants no test catches are listed in the job summary |
| `nightly.yml` | daily, or by hand | Flaky-test detection: `scripts/repeat-tests.sh` runs the unit suite (Linux and macOS) and the CouchDB suite 5 times and lists every test that failed in any run |

`bash scripts/repeat-tests.sh 5 bash scripts/test-couchdb.sh` does the same locally.

The blocking jobs use the toolchain pinned in `rust-toolchain.toml`. To move to a newer Rust, bump it there and fix any new lints in the same PR. The benchmarks can be run on a GitHub runner from the Actions tab (the manual "Benchmarks" workflow); shared runners are noisy, so use those numbers for trends only.
