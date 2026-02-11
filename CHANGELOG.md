# Changelog

All notable changes to RouchDB are documented here.

This project follows [Semantic Versioning](https://semver.org/). Since we are pre-1.0, minor version bumps may include breaking changes.

---

## [0.2.0] — 2026-02-07

Full PouchDB API parity release. This version adds every remaining PouchDB feature that was missing in 0.1.x, fixes several correctness bugs, and includes a new crate (`rouchdb-views`).

### Migration Guide (0.1.1 → 0.2.0)

#### Breaking Changes

1. **`Database` struct now has a `plugins` field.** If you destructure `Database`, update your code. Most users won't be affected since the fields are private.

2. **`ReplicationFilter::Custom` changed from `Box` to `Arc`:**
   ```rust
   // Before (0.1.1)
   ReplicationFilter::Custom(Box::new(|event| event.id.starts_with("user:")))

   // After (0.2.0)
   ReplicationFilter::Custom(Arc::new(|event| event.id.starts_with("user:")))
   ```

3. **`ReplicationOptions` has new fields.** If you construct it with struct literal syntax, add the new fields or use `..Default::default()`:
   ```rust
   // Recommended
   ReplicationOptions {
       batch_size: 50,
       ..Default::default()
   }
   ```
   New fields: `live`, `retry`, `poll_interval`, `back_off_function`, `since`, `checkpoint`.

4. **`AllDocsResponse` has a new optional `update_seq` field.** If you destructure it, add `update_seq`.

5. **`ChangeEvent` has a new optional `conflicts` field.** If you destructure it, add `conflicts`.

6. **`ChangesOptions` has new fields:** `conflicts`, `style` (`ChangesStyle` enum).

7. **`AllDocsOptions` has new fields:** `conflicts`, `update_seq`.

8. **`GetOptions` has new fields:** `revs_info`, `latest`, `attachments`.

9. **New `rouchdb-views` crate** is re-exported from the umbrella crate. No action needed unless you import crates individually.

10. **`build_path_from_revs` no longer panics on empty revs.** It now returns a degenerate single-node path. This is a behavior change that improves robustness.

#### New Dependencies

- `rouchdb-views` (new crate)
- `rouchdb-changes` now depends on `tokio-util` (for `CancellationToken`)
- `rouchdb-replication` now depends on `rouchdb-query`, `tokio-util`
- `rouchdb-core` now depends on `base64`
- `rouchdb-adapter-http` now depends on `percent-encoding`
- `rouchdb` (umbrella) now depends on `uuid`, `async-trait`

### New Features

#### Database API — New Methods

| Method | Description |
|--------|-------------|
| `post(data)` | Create a document with auto-generated UUID. Equivalent to PouchDB's `db.post()`. |
| `put_attachment(doc_id, att_id, rev, data, content_type)` | Store an attachment on a document. |
| `get_attachment(doc_id, att_id)` | Retrieve raw attachment bytes. |
| `get_attachment_with_opts(doc_id, att_id, opts)` | Retrieve attachment bytes with options (e.g., specific rev). |
| `remove_attachment(doc_id, att_id, rev)` | Remove an attachment from a document. |
| `create_index(def)` | Create a Mango index for faster queries. Returns `"created"` or `"exists"`. |
| `get_indexes()` | List all Mango indexes on this database. |
| `delete_index(name)` | Delete a Mango index by name. |
| `explain(opts)` | Analyze a query plan without executing it. |
| `live_changes(opts)` | Start a live changes stream → `(Receiver<ChangeEvent>, ChangesHandle)`. |
| `live_changes_events(opts)` | Live changes with lifecycle events → `(Receiver<ChangesEvent>, ChangesHandle)`. |
| `replicate_to_with_events(target, opts)` | One-shot replication with event streaming. |
| `replicate_to_live(target, opts)` | Continuous (live) replication → `(Receiver<ReplicationEvent>, ReplicationHandle)`. |
| `put_design(ddoc)` | Create or update a design document. |
| `get_design(name)` | Retrieve a design document by name. |
| `delete_design(name, rev)` | Delete a design document. |
| `view_cleanup()` | Remove orphaned view indexes. |
| `get_security()` | Get the database security document. |
| `put_security(doc)` | Set the database security document. |
| `close()` | Close the database and release resources. |
| `purge(doc_id, revs)` | Permanently remove document revisions (non-replicating). |
| `with_plugin(plugin)` | Register a plugin (builder pattern). |
| `partition(name)` | Get a partitioned view scoped to `"{name}:"` prefix. |
| `http_with_auth(url, auth)` | Connect to CouchDB with cookie authentication. |

#### Plugin System

New `Plugin` trait for extending database behavior with lifecycle hooks:

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;
    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> { Ok(()) }
    async fn after_write(&self, results: &[DocResult]) -> Result<()> { Ok(()) }
    async fn on_destroy(&self) -> Result<()> { Ok(()) }
}

let db = Database::memory("mydb").with_plugin(my_plugin);
```

#### Partitioned Queries

```rust
let partition = db.partition("users");
partition.all_docs(AllDocsOptions::new()).await?;
partition.find(FindOptions { selector: json!({"age": {"$gt": 21}}), ..Default::default() }).await?;
partition.get("alice").await?;  // fetches "users:alice"
partition.put("bob", json!({})).await?;  // stores "users:bob"
```

#### Live Replication

Continuous replication that polls for changes and syncs automatically:

```rust
let (mut rx, handle) = local.replicate_to_live(&remote, ReplicationOptions {
    poll_interval: Duration::from_secs(5),
    retry: true,
    ..Default::default()
});

while let Some(event) = rx.recv().await {
    match event {
        ReplicationEvent::Change { docs_read } => println!("synced {docs_read} docs"),
        ReplicationEvent::Paused => println!("up to date, waiting..."),
        ReplicationEvent::Error(msg) => eprintln!("error: {msg}"),
        _ => {}
    }
}

handle.cancel(); // or just drop it
```

#### Live Changes Feed

Two flavors — raw events or lifecycle-aware:

```rust
// Raw change events
let (mut rx, handle) = db.live_changes(ChangesStreamOptions::default());
while let Some(event) = rx.recv().await {
    println!("{}: {}", event.id, event.seq);
}

// Lifecycle events
let (mut rx, handle) = db.live_changes_events(ChangesStreamOptions::default());
while let Some(event) = rx.recv().await {
    match event {
        ChangesEvent::Change(ce) => println!("changed: {}", ce.id),
        ChangesEvent::Paused => println!("waiting..."),
        ChangesEvent::Active => println!("processing..."),
        ChangesEvent::Complete { last_seq } => println!("done at {last_seq}"),
        ChangesEvent::Error(msg) => eprintln!("{msg}"),
    }
}
```

Both support Mango selector filtering — only matching documents are forwarded through the channel.

#### Mango Indexes

Persistent in-memory indexes that speed up `find()` queries:

```rust
db.create_index(IndexDefinition {
    name: String::new(), // auto-generated as "idx-age"
    fields: vec![SortField::Simple("age".into())],
    ddoc: None,
}).await?;

// Queries on "age" now use the index
let result = db.find(FindOptions {
    selector: json!({"age": {"$gte": 21}}),
    ..Default::default()
}).await?;

// Inspect query plan
let plan = db.explain(FindOptions {
    selector: json!({"age": {"$gt": 20}}),
    ..Default::default()
}).await;
println!("Using index: {} ({})", plan.index.name, plan.index.index_type);
```

#### Cookie Authentication (HTTP Adapter)

```rust
use rouchdb::{AuthClient, Database};

let auth = AuthClient::new("http://localhost:5984");
auth.login("admin", "password").await?;

let db = Database::http_with_auth("http://localhost:5984/mydb", &auth);
```

#### Changes Feed Selector Filtering

```rust
// One-shot with selector
let changes = db.changes(ChangesOptions {
    selector: Some(json!({"type": "user"})),
    include_docs: true,
    ..Default::default()
}).await?;

// Live changes with selector
let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
    selector: Some(json!({"type": "user"})),
    ..Default::default()
});
```

#### New Core Types

| Type | Description |
|------|-------------|
| `PurgeResponse` | Result of a purge operation. |
| `SecurityDocument` / `SecurityGroup` | Database security configuration. |
| `RevInfo` | Revision info entry (rev + status). |
| `ChangesStyle` | Enum: `MainOnly` (default) or `AllDocs` (all leaf revisions). |
| `ChangesEvent` | Enum: `Change`, `Complete`, `Error`, `Paused`, `Active`. |
| `ChangesHandle` | Handle to cancel a live changes stream. |
| `ReplicationHandle` | Handle to cancel a live replication. |
| `IndexDefinition` / `IndexInfo` / `IndexFields` | Mango index types. |
| `CreateIndexResponse` / `BuiltIndex` | Index creation result and internal index state. |
| `ExplainResponse` / `ExplainIndex` | Query plan explanation types. |
| `DesignDocument` / `ViewDef` | Design document and view definition types. |
| `Plugin` trait | Lifecycle hooks for database operations. |
| `Partition` | Partitioned database view. |
| `AuthClient` / `Session` / `UserContext` | Cookie authentication types. |

#### New Adapter Trait Methods

| Method | Description |
|--------|-------------|
| `close()` | Release resources (default: no-op). |
| `purge(req)` | Permanently remove revisions. |
| `get_security()` | Get security document (default: empty). |
| `put_security(doc)` | Set security document (default: no-op). |

All new trait methods have default implementations, so existing custom adapters won't break.

#### New `rouchdb-views` Crate

Design documents and persistent view engine:
- `DesignDocument` struct with `views`, `filters`, `validate_doc_update`
- `ViewEngine` for persistent map/reduce indexes
- `PersistentViewIndex` for materialized view results

### Bug Fixes

- **Collation: `partial_cmp` → `total_cmp` for floats.** `f64::partial_cmp` returns `None` for NaN comparisons, which collapsed to `Equal`. Now uses `total_cmp` for well-defined ordering of all values including edge cases.
- **Collation: handle `NaN`, `Infinity`, `-Infinity` in number encoding.** The `encode_number` function panicked or produced incorrect sort keys for non-finite values. Now handles all IEEE 754 special values.
- **`build_path_from_revs` no longer panics on empty revs.** Returns a degenerate single-node path instead of hitting `assert!`.
- **`build_path_from_revs` uses saturating arithmetic.** Prevents underflow when `pos < len - 1`.
- **Attachment `from_json` parsing handles Base64 strings.** CouchDB sends inline attachment data as Base64 strings, but serde expected `Vec<u8>` (a JSON array). Now properly strips and decodes Base64 `data` fields.
- **`to_json` uses safe serialization for attachments.** Replaced `unwrap()` with `if let Ok(...)` to avoid panics on serialization edge cases.
- **ReDB `destroy()` uses O(1) table deletion.** Replaced `pop_last()` loops (O(n) per table) with `delete_table()` + `open_table()` for instant destruction regardless of database size.
- **Memory adapter: collapsible if-let chain.** Fixed Edition 2024 clippy lint.
- **`put()`, `update()`, `remove()` validate empty IDs.** Now return `RouchError::MissingId` instead of creating documents with empty IDs.
- **`put()`, `update()`, `remove()` now route through `bulk_docs()`.** This ensures plugins receive `before_write` / `after_write` hooks for all write operations.
- **`find()` rebuilds index on each query.** Indexes are lazily rebuilt before query execution to pick up document changes since the index was created.
- **HTTP adapter: URL-encode attachment IDs.** Attachment names with special characters (spaces, unicode) are now percent-encoded in HTTP requests.
- **HTTP adapter: `revs_info`, `latest`, `attachments` query parameters.** These `GetOptions` flags are now forwarded to CouchDB.

### Dependency Cleanup

- Removed 7 unused dependencies across 5 crates:
  - `rouchdb-core`: removed `md-5`, `uuid`
  - `rouchdb-changes`: removed `tokio-stream`, `async-trait`
  - `rouchdb-replication`: removed `rouchdb-changes`
  - `rouchdb-adapter-redb`: removed `base64`
  - `rouchdb-views`: removed `async-trait`

### Documentation

- Corrected 20+ inaccuracies across the mdBook documentation
- Fixed API signatures, type names, and code examples to match actual implementation
- Added `rouchdb-views` crate to installation guide and crate guide
- Updated dependency graph in CLAUDE.md and crate-guide.md
- Fixed Spanish translation of replication guide (missing fields, type errors)
- Added new chapters: live replication, live changes, design documents
- All mdBook builds cleanly with no warnings

### Tests

- 287 unit tests passing across all 9 crates
- New test suites: `post_and_attachments.rs`, expanded `changes_feed.rs`, expanded `mango_queries.rs`, expanded `replication.rs`
- Zero clippy warnings

---

## [0.1.1] — 2026-02-06

Version bump for crates.io publishing with updated metadata and repository URLs.

### Changes

- Bump version to 0.1.1 across all workspace crates
- Add MIT license file and crates.io publish metadata (description, keywords, categories)
- Update repository URL to `github.com/RubyLabApp/rouchdb`
- Add CI workflow (build, test, clippy, fmt) and docs deployment workflow (GitHub Actions)
- Fix all clippy warnings for clean CI
- Add CLAUDE.md and README.md
- Track `docker-compose.yml` for CouchDB integration tests
- Add filtered replication (doc_ids, selector, custom closure filters)

---

## [0.1.0] — 2026-02-05

Initial release. Core document database with CouchDB replication.

### Crates

| Crate | Description |
|-------|-------------|
| `rouchdb-core` | Types, traits, revision tree, merge algorithm, CouchDB-compatible collation, errors |
| `rouchdb-adapter-memory` | In-memory adapter for testing and ephemeral data |
| `rouchdb-adapter-redb` | Persistent local storage via redb (pure Rust, no C deps) |
| `rouchdb-adapter-http` | CouchDB HTTP client adapter via reqwest |
| `rouchdb-changes` | Changes feed with one-shot and live streaming modes |
| `rouchdb-replication` | CouchDB replication protocol with checkpoint-based incremental sync |
| `rouchdb-query` | Mango selectors (`$eq`, `$gt`, `$in`, `$regex`, etc.) and map/reduce views |
| `rouchdb` | Umbrella crate with `Database` API and re-exports |

### Features

- **Document CRUD:** `put`, `get`, `get_with_opts`, `update`, `remove`, `bulk_docs`, `all_docs`
- **Revision Tree:** Full CouchDB-compatible revision tree with merge algorithm and deterministic winning revision selection
- **Collation:** CouchDB-compatible ordering (null < bool < number < string < array < object)
- **Changes Feed:** One-shot and live streaming with doc_ids filtering
- **Replication:** Complete CouchDB replication protocol — checkpoints, revs_diff, bulk_get, new_edits=false
- **Mango Queries:** `find()` with selectors, field projection, sorting, skip/limit
- **Map/Reduce:** `query_view()` with custom map functions and built-in reduce (`_count`, `_sum`, `_stats`)
- **Attachments:** Binary attachment storage and retrieval via adapter trait
- **Multiple Backends:** Memory (testing), redb (persistent), HTTP (CouchDB remote)
- **Bidirectional Sync:** `sync()` method for push + pull in one call
- **Seq Type:** Handles both numeric (local) and opaque string (CouchDB 3.x) sequences
- **mdBook Documentation:** Guides, reference, architecture docs, and Spanish translations
- **77 integration tests** against real CouchDB
- **85%+ unit test coverage** across all crates
