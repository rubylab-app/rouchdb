# Adapter Trait Reference

The `Adapter` trait is the storage abstraction at the heart of RouchDB. Every storage backend -- in-memory, redb, CouchDB HTTP -- implements this trait. The high-level [`Database`](database-api.md) struct delegates all operations to its underlying `Adapter`.

This trait mirrors PouchDB's internal adapter interface (the underscore-prefixed methods in JavaScript), where each method corresponds to a CouchDB API endpoint.

```rust
use rouchdb_core::adapter::Adapter;
```

---

## Full Trait Definition

```rust
#[async_trait]
pub trait Adapter: Send + Sync {
    async fn info(&self) -> Result<DbInfo>;

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document>;

    async fn bulk_docs(
        &self,
        docs: Vec<Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>>;

    async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse>;

    async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse>;

    async fn revs_diff(
        &self,
        revs: HashMap<String, Vec<String>>,
    ) -> Result<RevsDiffResponse>;

    async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse>;

    async fn put_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<DocResult>;

    async fn get_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>>;

    async fn remove_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
    ) -> Result<DocResult>;

    async fn get_local(&self, id: &str) -> Result<serde_json::Value>;

    async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()>;

    async fn remove_local(&self, id: &str) -> Result<()>;

    async fn compact(&self) -> Result<()>;

    async fn destroy(&self) -> Result<()>;

    // Methods with default implementations:

    async fn close(&self) -> Result<()> { Ok(()) }

    async fn purge(
        &self,
        req: HashMap<String, Vec<String>>,
    ) -> Result<PurgeResponse> { /* default: error */ }

    async fn get_security(&self) -> Result<SecurityDocument> { /* default: empty */ }

    async fn put_security(&self, doc: SecurityDocument) -> Result<()> { /* default: no-op */ }
}
```

---

## Method Reference

### Database Information

| Method | Signature | Description |
|--------|-----------|-------------|
| `info` | `async fn info(&self) -> Result<DbInfo>` | Returns the database name, document count, and current update sequence. |

**Behavior contract:** Must always succeed for a valid, non-destroyed database. The `update_seq` should reflect the latest write.

---

### Document Retrieval

| Method | Signature | Description |
|--------|-----------|-------------|
| `get` | `async fn get(&self, id: &str, opts: GetOptions) -> Result<Document>` | Retrieve a single document by its `_id`. |

**Behavior contract:**

- With default `GetOptions`: returns the winning revision of the document. Returns `RouchError::NotFound` if the document does not exist or the winning revision is a deletion.
- With `opts.rev = Some(rev)`: returns the specific revision, even if it is not the winner. Returns `NotFound` if that revision does not exist.
- With `opts.conflicts = true`: the returned document includes information about conflicting leaf revisions.
- With `opts.open_revs = Some(OpenRevs::All)`: returns all leaf revisions of the document.
- With `opts.revs = true`: includes full revision history in the response.

**When it is called:** Every `Database::get` and `Database::get_with_opts` call delegates here. Also called internally during replication to fetch specific revisions.

---

### Document Writing

| Method | Signature | Description |
|--------|-----------|-------------|
| `bulk_docs` | `async fn bulk_docs(&self, docs: Vec<Document>, opts: BulkDocsOptions) -> Result<Vec<DocResult>>` | Write multiple documents in a single atomic operation. |

**Behavior contract:**

This method has two distinct modes controlled by `opts.new_edits`:

#### User mode (`new_edits: true` -- the default)

This is the mode used for normal application writes. The adapter:

1. **Generates a new revision ID** for each document (incrementing the `pos` and computing a new hash).
2. **Checks for conflicts** -- if the document already exists, the provided `_rev` must match the current winning revision. If it does not match, the adapter returns a `DocResult` with `ok: false` and an error of `"conflict"`.
3. **Appends the new revision** as a child of the provided parent revision in the document's revision tree.
4. **Increments the database sequence number** for each successful write.

#### Replication mode (`new_edits: false`)

This is the mode used by the replication protocol. The adapter:

1. **Accepts revision IDs as provided** -- does not generate new revisions.
2. **Does not check for conflicts** -- merges the incoming revision tree with the existing one.
3. **Inserts the revision into the tree** at the correct position, potentially creating new branches.
4. **Determines the new winning revision** using the deterministic algorithm (highest `pos`, then lexicographic hash comparison).

This distinction is critical for correct replication behavior. When documents arrive from a remote peer, they carry their own revision history and must be merged without conflict rejection.

**When it is called:** All `Database::put`, `Database::update`, `Database::remove`, and `Database::bulk_docs` calls delegate here. The replication protocol calls this with `BulkDocsOptions::replication()`.

---

### Querying

| Method | Signature | Description |
|--------|-----------|-------------|
| `all_docs` | `async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse>` | Query all documents, optionally filtered by key range. |
| `changes` | `async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse>` | Get changes since a given sequence number. |

**`all_docs` behavior contract:**

- Returns documents sorted by `_id` in CouchDB collation order.
- Supports key range filtering via `start_key`, `end_key`, `key`, and `keys`.
- When `include_docs` is true, the full document body is included in each row.
- Deleted documents are excluded from results unless requested by specific key.
- Supports `descending` order, `skip`, and `limit` for pagination.

**`changes` behavior contract:**

- Returns change events in sequence order, starting after `opts.since`.
- Each `ChangeEvent` contains the document `_id`, the sequence number, and the list of changed revisions.
- When `include_docs` is true, the full document body is included.
- Supports `limit` to cap the number of results, `descending` for reverse order, and `doc_ids` to filter by specific document IDs.
- The `last_seq` in the response can be passed as `since` in the next call for incremental polling.

**When they are called:** `all_docs` is called by `Database::all_docs` and internally by the Mango query engine and map/reduce views. `changes` is called by `Database::changes` and is the starting point of every replication cycle.

---

### Replication Support

| Method | Signature | Description |
|--------|-----------|-------------|
| `revs_diff` | `async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse>` | Compare document revision sets against the adapter's local state to determine which revisions are missing. |
| `bulk_get` | `async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse>` | Fetch multiple documents by ID and optionally by specific revision in a single request. |

**`revs_diff` behavior contract:**

- Input: a map of document IDs to lists of revision strings.
- Output: for each document, the list of revisions the adapter does **not** have, plus any `possible_ancestors` (revisions the adapter does have that could be ancestors of the missing ones).
- This avoids transferring documents the target already has during replication.

**`bulk_get` behavior contract:**

- Input: a list of `BulkGetItem` structs, each with an `id` and an optional `rev`.
- Output: for each requested item, the document JSON (in `ok`) or an error (in `error`).
- When `rev` is `None`, the winning revision is returned.
- Used during replication to efficiently fetch all missing documents in a single round trip.

**When they are called:** Both are called by the replication protocol. `revs_diff` is called in step 3 (after fetching changes from the source), and `bulk_get` is called in step 4 (to fetch the actual missing documents).

---

### Attachments

| Method | Signature | Description |
|--------|-----------|-------------|
| `put_attachment` | `async fn put_attachment(&self, doc_id: &str, att_id: &str, rev: &str, data: Vec<u8>, content_type: &str) -> Result<DocResult>` | Store binary attachment data on a document. |
| `get_attachment` | `async fn get_attachment(&self, doc_id: &str, att_id: &str, opts: GetAttachmentOptions) -> Result<Vec<u8>>` | Retrieve raw binary attachment data. |
| `remove_attachment` | `async fn remove_attachment(&self, doc_id: &str, att_id: &str, rev: &str) -> Result<DocResult>` | Remove an attachment from a document. Creates a new revision. |

**`put_attachment` behavior contract:**

- Requires a valid current `rev` for the document (same conflict semantics as `bulk_docs` in user mode).
- Creates or replaces the named attachment.
- Returns a `DocResult` with the new revision.

**`get_attachment` behavior contract:**

- Returns the raw bytes of the attachment.
- Optionally accepts a specific revision via `GetAttachmentOptions`.
- Returns `RouchError::NotFound` if the document, revision, or attachment does not exist.

---

### Local Documents

Local documents are a special class of documents that are **never replicated**. They exist only in the local adapter and are used primarily for storing replication checkpoints.

| Method | Signature | Description |
|--------|-----------|-------------|
| `get_local` | `async fn get_local(&self, id: &str) -> Result<serde_json::Value>` | Retrieve a local document by its ID. |
| `put_local` | `async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()>` | Write a local document. Creates or overwrites. |
| `remove_local` | `async fn remove_local(&self, id: &str) -> Result<()>` | Delete a local document. |

**Behavior contract:**

- Local documents do not participate in the changes feed.
- Local documents do not have revision trees -- they are simple key-value entries.
- The `id` does not need the `_local/` prefix; the adapter handles namespacing internally.
- `get_local` returns `RouchError::NotFound` if the local document does not exist.

**Role in replication checkpoints:**

The replication protocol uses local documents to store checkpoints on both the source and target databases. The checkpoint contains the last replicated sequence number, enabling incremental replication:

```rust
// The replication protocol does this internally:
let checkpoint_id = "_local/replication-checkpoint-abc123";

// Read the last checkpoint
let last_seq = adapter.get_local(checkpoint_id).await
    .map(|doc| doc["last_seq"].clone())
    .unwrap_or(Seq::zero());

// After replicating, save the new checkpoint
adapter.put_local(checkpoint_id, json!({
    "last_seq": current_seq,
    "session_id": session_id,
})).await?;
```

This is why local documents are excluded from replication -- each side maintains its own checkpoint independently.

---

### Maintenance

| Method | Signature | Description |
|--------|-----------|-------------|
| `compact` | `async fn compact(&self) -> Result<()>` | Remove old (non-leaf) revisions and clean up unreferenced attachment data. |
| `destroy` | `async fn destroy(&self) -> Result<()>` | Destroy the database and all its data. After calling this, the adapter should not be used. |
| `close` | `async fn close(&self) -> Result<()>` | Release resources (default: no-op). |
| `purge` | `async fn purge(&self, req: HashMap<String, Vec<String>>) -> Result<PurgeResponse>` | Permanently remove specific revisions. Purged revisions do not replicate. Default returns an error. |
| `get_security` | `async fn get_security(&self) -> Result<SecurityDocument>` | Get the database security document (default: empty document). |
| `put_security` | `async fn put_security(&self, doc: SecurityDocument) -> Result<()>` | Set the database security document (default: no-op). |

**When they are called:** `compact` is called by `Database::compact`, typically as a periodic maintenance task. `destroy` is called by `Database::destroy` when the user wants to permanently delete the database. `close`, `purge`, `get_security`, and `put_security` have default implementations so existing adapters don't need to implement them.

---

## Built-in Adapter Implementations

RouchDB ships with three adapter implementations:

| Adapter | Crate | Description |
|---------|-------|-------------|
| `MemoryAdapter` | `rouchdb-adapter-memory` | In-memory storage. Fast, no persistence. Ideal for tests. |
| `RedbAdapter` | `rouchdb-adapter-redb` | Persistent storage backed by [redb](https://github.com/cberner/redb). Pure Rust, no C dependencies. |
| `HttpAdapter` | `rouchdb-adapter-http` | Remote CouchDB client using HTTP/JSON via [reqwest](https://docs.rs/reqwest). |

---

## Implementing a Custom Adapter

To implement your own storage backend, implement all methods of the `Adapter` trait:

```rust
use async_trait::async_trait;
use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::Result;
use std::collections::HashMap;

pub struct MyAdapter { /* ... */ }

#[async_trait]
impl Adapter for MyAdapter {
    async fn info(&self) -> Result<DbInfo> {
        // ...
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        // ...
    }

    // ... implement all other methods
}
```

Then use it with `Database`:

```rust
let adapter = Arc::new(MyAdapter::new());
let db = Database::from_adapter(adapter);
```
