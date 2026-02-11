# Adapters

RouchDB uses an adapter pattern to separate the database API from the underlying storage engine. The `Database` struct wraps any implementation of the `Adapter` trait, so the same high-level API works identically whether data lives in memory, on disk, or on a remote CouchDB server.

## Built-In Adapters

RouchDB ships with three adapters. Each has a convenience constructor on the `Database` type.

### MemoryAdapter

Stores everything in memory. Data is lost when the `Database` is dropped.

```rust
use rouchdb::Database;

let db = Database::memory("mydb");
```

**When to use:**
- Unit and integration tests.
- Temporary scratch databases.
- Prototyping without setting up storage.
- As a replication target for in-process data transformation.

### RedbAdapter

Persistent storage backed by [redb](https://github.com/cberner/redb), a pure-Rust embedded key-value store. No C dependencies, no FFI, no runtime configuration.

```rust
use rouchdb::Database;

let db = Database::open("path/to/mydb.redb", "mydb")?;
```

The first argument is the filesystem path for the redb file. The second is the logical database name (used in replication checkpoints and `db.info()`).

**When to use:**
- Production local-first applications.
- Any scenario where data must survive process restarts.
- Offline-capable applications that sync when connectivity returns.

### HttpAdapter

Connects to a remote CouchDB (or compatible) server over HTTP. All operations are translated to CouchDB REST API calls.

```rust
use rouchdb::Database;

// Without authentication
let db = Database::http("http://localhost:5984/mydb");

// With basic auth embedded in the URL
let db = Database::http("http://admin:password@localhost:5984/mydb");
```

**When to use:**
- Connecting to a CouchDB cluster.
- Using CouchDB as the "source of truth" server.
- As a replication source or target.
- When you need CouchDB's built-in features (Mango indexes, design documents, etc.) directly.

## Choosing an Adapter

| Scenario | Adapter |
|----------|---------|
| Tests | `Database::memory()` |
| Desktop / mobile / CLI app | `Database::open()` (redb) |
| Server talking to CouchDB | `Database::http()` |
| Local-first with sync | `Database::open()` locally, `Database::http()` for the remote, then `sync()` |

## Using Database::from_adapter

If you have an adapter instance created outside the convenience constructors (for example, a custom adapter or one configured with special options), use `from_adapter`:

```rust
use std::sync::Arc;
use rouchdb::{Database, MemoryAdapter};

let adapter = Arc::new(MemoryAdapter::new("custom"));
let db = Database::from_adapter(adapter);

// Works exactly like Database::memory("custom")
db.put("doc1", serde_json::json!({"hello": "world"})).await?;
```

## Accessing the Underlying Adapter

You can get a reference to the underlying adapter for operations that go through the `Adapter` trait directly:

```rust
let db = Database::memory("mydb");

// Returns &dyn Adapter
let adapter = db.adapter();

// Use adapter methods directly (e.g., for map/reduce)
use rouchdb::{query_view, ViewQueryOptions};

let result = query_view(
    adapter,
    &|doc| {
        let name = doc.get("name").cloned().unwrap_or(serde_json::json!(null));
        vec![(name, serde_json::json!(1))]
    },
    None,
    ViewQueryOptions::new(),
).await?;
```

This is particularly useful for `query_view()`, attachment operations, and local document storage, which take an `&dyn Adapter` parameter.

## Implementing a Custom Adapter

To create your own storage backend, implement the `Adapter` trait from `rouchdb_core`. Here is the full trait signature:

```rust
use async_trait::async_trait;
use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::Result;
use std::collections::HashMap;

pub struct MyAdapter {
    // your storage state
}

#[async_trait]
impl Adapter for MyAdapter {
    async fn info(&self) -> Result<DbInfo> { todo!() }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> { todo!() }

    async fn bulk_docs(
        &self,
        docs: Vec<Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>> { todo!() }

    async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> { todo!() }

    async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> { todo!() }

    async fn revs_diff(
        &self,
        revs: HashMap<String, Vec<String>>,
    ) -> Result<RevsDiffResponse> { todo!() }

    async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> { todo!() }

    async fn put_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<DocResult> { todo!() }

    async fn get_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>> { todo!() }

    async fn remove_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
    ) -> Result<DocResult> { todo!() }

    async fn get_local(&self, id: &str) -> Result<serde_json::Value> { todo!() }

    async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> { todo!() }

    async fn remove_local(&self, id: &str) -> Result<()> { todo!() }

    async fn compact(&self) -> Result<()> { todo!() }

    async fn destroy(&self) -> Result<()> { todo!() }

    // close, purge, get_security, put_security have default implementations
    // Override them if your adapter needs custom behavior.
}
```

### Key Implementation Notes

**bulk_docs** is the most complex method. When `opts.new_edits` is `true`, you must:
- Generate new revision IDs for each document.
- Check that the provided `_rev` matches the current winning revision (otherwise return a conflict error).
- Merge the new revision into the document's revision tree.

When `opts.new_edits` is `false` (replication mode), you must:
- Accept revision IDs as-is without generating new ones.
- Merge incoming revisions into the existing revision tree using `merge_tree()` from `rouchdb_core::merge`.
- Never reject a write due to conflicts.

**changes** must return events ordered by sequence number. Each write to the database increments the sequence.

**get_local / put_local / remove_local** manage local documents (prefixed with `_local/` in CouchDB). These are used by the replication protocol to store checkpoints. They do not participate in the changes feed or replication.

**revs_diff** is used during replication. Given a map of `{doc_id: [rev1, rev2, ...]}`, return which revisions the adapter does not have. This avoids transferring documents the target already has.

### Using Your Custom Adapter

```rust
use std::sync::Arc;
use rouchdb::Database;

let my_adapter = Arc::new(MyAdapter::new(/* ... */));
let db = Database::from_adapter(my_adapter);

// Now use db normally -- put, get, replicate, etc.
db.put("doc1", serde_json::json!({"works": true})).await?;
```

Because the `Database` type erases the adapter behind `Arc<dyn Adapter>`, all RouchDB features (replication, queries, changes feed) work transparently with any conforming adapter.
