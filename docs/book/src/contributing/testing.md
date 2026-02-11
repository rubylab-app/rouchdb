# Testing

RouchDB has two categories of tests: **unit tests** that run without external dependencies, and **integration tests** that require a running CouchDB instance.

## Unit Tests

Unit tests are defined as `#[cfg(test)]` modules inside each crate's source files. They cover internal logic without any external services.

### Running All Unit Tests

```bash
cargo test
```

This runs every unit test across all 9 workspace crates.

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

Integration tests live in `crates/rouchdb/tests/` across multiple test files (`http_crud.rs`, `replication.rs`, `changes_feed.rs`, `mango_queries.rs`, etc.). They verify RouchDB against a real CouchDB server to ensure protocol compliance and end-to-end correctness.

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

All integration tests are marked `#[ignore]` so they are skipped during `cargo test`. Run them with:

```bash
cargo test -p rouchdb --test '*' -- --ignored
```

To run a single integration test by name:

```bash
cargo test -p rouchdb --test http_crud http_put_and_get -- --ignored
```

### Custom CouchDB URL

To point tests at a different CouchDB instance, set the `COUCHDB_URL` environment variable:

```bash
COUCHDB_URL="http://user:pass@myhost:5984" \
  cargo test -p rouchdb --test '*' -- --ignored
```

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

Integration tests go in `crates/rouchdb/tests/` as separate test files. They test the high-level `Database` API against a real CouchDB instance.

### Structure of an Integration Test

Every integration test follows this pattern:

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

- **Always add `#[ignore]`** so the test does not run in `cargo test`.
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

The `MemoryAdapter` is the primary tool for fast, isolated unit tests. It implements the full `Adapter` trait in memory with no I/O, making tests instant and deterministic.

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

The integration test files share three common helpers:

- `couchdb_url()` -- Returns the CouchDB base URL, respecting the `COUCHDB_URL` environment variable.
- `fresh_remote_db(prefix)` -- Creates a new CouchDB database with a UUID-based name and returns its URL.
- `delete_remote_db(url)` -- Deletes a CouchDB database by URL.

## Continuous Integration

When running tests in CI, use two stages:

```bash
# Stage 1: Unit tests (no services needed)
cargo test

# Stage 2: Integration tests (CouchDB required)
docker compose up -d --wait
cargo test -p rouchdb --test '*' -- --ignored
docker compose down
```

The `--wait` flag tells Docker Compose to block until the health check passes before proceeding.
