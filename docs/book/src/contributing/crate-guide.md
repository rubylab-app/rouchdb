# Crate Guide

RouchDB is structured as a Cargo workspace with 9 crates. This guide helps you figure out where to add new code.

## Where Do I Add Code?

Use this decision tree to find the right crate:

```
Is it a new document type, revision type, or error variant?
  --> rouchdb-core

Is it a new storage backend?
  --> New crate implementing the Adapter trait (see "Adding an Adapter" below)

Is it a change to how queries work (Mango selectors, map/reduce)?
  --> rouchdb-query

Is it a change to design documents or the persistent view engine?
  --> rouchdb-views

Is it a change to the replication protocol or checkpointing?
  --> rouchdb-replication

Is it a change to the changes feed or live streaming?
  --> rouchdb-changes

Is it a change to the high-level Database API?
  --> rouchdb (the umbrella crate)

Is it a change to how CouchDB HTTP communication works?
  --> rouchdb-adapter-http

Is it a change to the in-memory storage implementation?
  --> rouchdb-adapter-memory

Is it a change to the persistent redb storage implementation?
  --> rouchdb-adapter-redb
```

## Crate Descriptions

### `rouchdb-core`

The foundation crate. Everything else depends on it.

**Responsibility:** Core types, traits, and algorithms shared across the entire project.

**Key files:**
- `src/adapter.rs` -- The `Adapter` trait that all storage backends implement. Defines methods like `get`, `bulk_docs`, `all_docs`, `changes`, `revs_diff`, `bulk_get`, and local document operations.
- `src/document.rs` -- Document types (`Document`, `Revision`, `DocResult`, `DbInfo`, `AllDocsResponse`, `ChangesResponse`, `Seq`, etc.).
- `src/rev_tree.rs` -- Revision tree data structures (`RevTree`, `RevPath`, `RevNode`) and tree traversal utilities.
- `src/merge.rs` -- Revision tree merging algorithm, winning revision selection (`winning_rev`), and conflict detection.
- `src/collation.rs` -- CouchDB-compatible collation ordering for view keys.
- `src/error.rs` -- `RouchError` enum and `Result` type alias.

### `rouchdb-adapter-memory`

**Responsibility:** In-memory adapter for fast testing and ephemeral databases. Stores documents in `HashMap` behind a `RwLock`.

**Key files:**
- `src/lib.rs` -- Full `Adapter` trait implementation with `StoredDoc` internal structure, revision tree management, and sequence tracking.

### `rouchdb-adapter-redb`

**Responsibility:** Persistent local storage using [redb](https://github.com/cberner/redb), a pure-Rust embedded key-value store.

**Key files:**
- `src/lib.rs` -- `RedbAdapter` struct implementing the `Adapter` trait with durable on-disk storage.

### `rouchdb-adapter-http`

**Responsibility:** Remote CouchDB communication. Maps each `Adapter` method to the corresponding CouchDB REST API endpoint using `reqwest`.

**Key files:**
- `src/lib.rs` -- `HttpAdapter` struct, CouchDB JSON response deserialization types, and the full `Adapter` trait implementation.

### `rouchdb-changes`

**Responsibility:** Streaming changes feed with one-shot and live/continuous modes.

**Key files:**
- `src/lib.rs` -- `ChangeSender`, `ChangeReceiver`, `ChangeNotification`, and `LiveChangesStream`. Uses Tokio broadcast channels for real-time change notification.

### `rouchdb-replication`

**Responsibility:** CouchDB replication protocol implementation. Handles checkpoint-based incremental replication between any two adapters.

**Key files:**
- `src/protocol.rs` -- The core replication loop: read changes from source, diff against target, fetch missing documents, write to target.
- `src/checkpoint.rs` -- Checkpoint management using `_local` documents to track replication progress.
- `src/lib.rs` -- Public API: `replicate()` function, `ReplicationOptions`, `ReplicationResult`, `ReplicationEvent`.

### `rouchdb-query`

**Responsibility:** Mango query selectors and map/reduce view queries.

**Key files:**
- `src/mango.rs` -- Mango selector parsing and matching (`matches_selector`). Supports operators like `$eq`, `$gt`, `$in`, `$regex`, etc.
- `src/mapreduce.rs` -- Map/reduce view execution (`query_view`). Takes a map function, optional reduce function, and query options.
- `src/lib.rs` -- Re-exports: `find`, `FindOptions`, `FindResponse`, `ViewQueryOptions`, `ViewResult`, `ReduceFn`, `SortField`.

### `rouchdb-views`

**Responsibility:** Design documents and the persistent view engine.

**Key files:**
- `src/lib.rs` -- `DesignDocument` struct (with views, filters, validate_doc_update) and `ViewEngine` for persistent map/reduce indexes.

### `rouchdb`

**Responsibility:** The umbrella crate that end users depend on. Provides the high-level `Database` struct and re-exports types from all other crates.

**Key files:**
- `src/lib.rs` -- `Database` struct with user-friendly methods (`put`, `get`, `update`, `remove`, `replicate_to`, `replicate_from`, `find`, `all_docs`, `changes`). Wraps any `Adapter` behind an `Arc<dyn Adapter>`.
- `tests/*.rs` -- Integration tests against a real CouchDB instance (multiple test files: `replication.rs`, `http_crud.rs`, `changes_feed.rs`, etc.).

## Adding an Adapter

To add a new storage backend (e.g., SQLite, IndexedDB via wasm):

1. Create a new crate: `crates/rouchdb-adapter-yourbackend/`
2. Add it to the workspace `members` list in the root `Cargo.toml`
3. Depend on `rouchdb-core` for the `Adapter` trait and document types
4. Implement the `Adapter` trait from `rouchdb_core::adapter::Adapter`
5. Add the crate as a dependency in the umbrella `rouchdb` crate and re-export it

The `Adapter` trait requires implementing these async methods:

- `info()` -- Database metadata
- `get()` -- Fetch a single document
- `bulk_docs()` -- Write multiple documents atomically
- `all_docs()` -- List/query all documents
- `changes()` -- Get changes since a sequence
- `revs_diff()` -- Compare revision sets (for replication)
- `bulk_get()` -- Fetch multiple documents by ID and revision
- `get_local()` / `put_local()` / `remove_local()` -- Local (non-replicated) document operations
- `get_attachment()` / `put_attachment()` / `remove_attachment()` -- Attachment storage
- `close()` -- Clean up resources

Look at `rouchdb-adapter-memory` as a reference implementation -- it is the simplest complete adapter.

## Dependency Graph

```
rouchdb (umbrella)
  |-- rouchdb-core
  |-- rouchdb-adapter-memory   --> rouchdb-core
  |-- rouchdb-adapter-redb     --> rouchdb-core
  |-- rouchdb-adapter-http     --> rouchdb-core
  |-- rouchdb-changes          --> rouchdb-core
  |-- rouchdb-query            --> rouchdb-core
  |-- rouchdb-views            --> rouchdb-core
  |-- rouchdb-replication      --> rouchdb-core, rouchdb-query
```

All crates depend on `rouchdb-core`. The umbrella `rouchdb` crate depends on all of them and re-exports their public APIs.
