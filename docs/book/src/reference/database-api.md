# Database API Reference

The `Database` struct is the primary entry point for all RouchDB operations. It wraps any [`Adapter`](adapter-trait.md) implementation behind an `Arc<dyn Adapter>`, providing a high-level API similar to PouchDB's JavaScript interface.

```rust
use rouchdb::Database;
```

---

## Constructors

| Method | Signature | Description |
|--------|-----------|-------------|
| `memory` | `fn memory(name: &str) -> Self` | Create an in-memory database. Data is lost when the `Database` is dropped. Useful for testing. |
| `open` | `fn open(path: impl AsRef<Path>, name: &str) -> Result<Self>` | Open or create a persistent database backed by [redb](https://github.com/cberner/redb). Returns an error if the file cannot be opened or created. |
| `http` | `fn http(url: &str) -> Self` | Connect to a remote CouchDB-compatible server. The URL should include the database name (e.g., `http://localhost:5984/mydb`). |
| `from_adapter` | `fn from_adapter(adapter: Arc<dyn Adapter>) -> Self` | Create a `Database` from any custom adapter implementation. Use this when you need to provide your own storage backend. |

### Examples

```rust
// In-memory (for tests)
let db = Database::memory("mydb");

// Persistent (redb file)
let db = Database::open("path/to/mydb.redb", "mydb")?;

// Remote CouchDB
let db = Database::http("http://localhost:5984/mydb");
```

---

## Document Operations

These methods correspond to CouchDB's core document API. See the [Core Types Reference](core-types.md) for details on the option and response structs.

| Method | Signature | Return Type | Description |
|--------|-----------|-------------|-------------|
| `info` | `async fn info(&self)` | `Result<DbInfo>` | Get database metadata: name, document count, and current update sequence. |
| `get` | `async fn get(&self, id: &str)` | `Result<Document>` | Retrieve a document by its `_id`. Returns `RouchError::NotFound` if the document does not exist or has been deleted. |
| `get_with_opts` | `async fn get_with_opts(&self, id: &str, opts: GetOptions)` | `Result<Document>` | Retrieve a document with options: specific revision, conflict info, all open revisions, or full revision history. |
| `post` | `async fn post(&self, data: serde_json::Value)` | `Result<DocResult>` | Create a new document with an auto-generated UUID v4 as the ID. Equivalent to PouchDB's `db.post()`. |
| `put` | `async fn put(&self, id: &str, data: serde_json::Value)` | `Result<DocResult>` | Create a new document. If a document with the same `_id` already exists and has no previous revision, this creates it; otherwise it may conflict. |
| `update` | `async fn update(&self, id: &str, rev: &str, data: serde_json::Value)` | `Result<DocResult>` | Update an existing document. You must provide the current `_rev` string. Returns `RouchError::Conflict` if the rev does not match. |
| `remove` | `async fn remove(&self, id: &str, rev: &str)` | `Result<DocResult>` | Delete a document by marking it as deleted. Requires the current `_rev`. The document remains in the database as a deletion tombstone. |
| `bulk_docs` | `async fn bulk_docs(&self, docs: Vec<Document>, opts: BulkDocsOptions)` | `Result<Vec<DocResult>>` | Write multiple documents atomically. See [`BulkDocsOptions`](core-types.md) for user mode vs. replication mode. |
| `all_docs` | `async fn all_docs(&self, opts: AllDocsOptions)` | `Result<AllDocsResponse>` | Query all documents, optionally filtered by key range. Supports pagination, descending order, and including full document bodies. |
| `changes` | `async fn changes(&self, opts: ChangesOptions)` | `Result<ChangesResponse>` | Get the list of changes since a given sequence. Used for change tracking, live feeds, and replication. |

### Examples

```rust
// Post (auto-generated ID)
let result = db.post(json!({"name": "Alice", "age": 30})).await?;
println!("Generated ID: {}", result.id);

// Put (explicit ID)
let result = db.put("user:alice", json!({"name": "Alice", "age": 30})).await?;
let doc = db.get("user:alice").await?;

// Update (requires current rev)
let updated = db.update("user:alice", &result.rev.unwrap(), json!({"name": "Alice", "age": 31})).await?;

// Delete
db.remove("user:alice", &updated.rev.unwrap()).await?;

// Bulk write
let docs = vec![
    Document { id: "a".into(), rev: None, deleted: false, data: json!({}), attachments: HashMap::new() },
    Document { id: "b".into(), rev: None, deleted: false, data: json!({}), attachments: HashMap::new() },
];
let results = db.bulk_docs(docs, BulkDocsOptions::new()).await?;

// All docs with options
let response = db.all_docs(AllDocsOptions {
    include_docs: true,
    limit: Some(10),
    ..AllDocsOptions::new()
}).await?;
```

---

## Attachment Operations

| Method | Signature | Return Type | Description |
|--------|-----------|-------------|-------------|
| `remove_attachment` | `async fn remove_attachment(&self, doc_id: &str, att_id: &str, rev: &str)` | `Result<DocResult>` | Remove an attachment from a document. Creates a new revision with the attachment removed. Equivalent to PouchDB's `db.removeAttachment()`. |

### Example

```rust
// Put and get attachment via adapter
let att_result = db.adapter()
    .put_attachment("doc1", "photo.jpg", &rev, data, "image/jpeg")
    .await?;

// Remove the attachment
let rm = db.remove_attachment("doc1", "photo.jpg", &att_result.rev.unwrap()).await?;
```

---

## Query Operations

| Method | Signature | Return Type | Description |
|--------|-----------|-------------|-------------|
| `find` | `async fn find(&self, opts: FindOptions)` | `Result<FindResponse>` | Run a Mango find query with selectors, field projection, sorting, and pagination. If a matching index exists, it will be used. See [`FindOptions`](core-types.md). |

### Example

```rust
let result = db.find(FindOptions {
    selector: json!({"age": {"$gte": 21}}),
    fields: Some(vec!["name".into(), "age".into()]),
    sort: Some(vec![SortField::Simple("age".into())]),
    limit: Some(25),
    ..Default::default()
}).await?;

for doc in &result.docs {
    println!("{}", doc);
}
```

---

## Index Operations

| Method | Signature | Return Type | Description |
|--------|-----------|-------------|-------------|
| `create_index` | `async fn create_index(&self, def: IndexDefinition)` | `Result<CreateIndexResponse>` | Create a Mango index for faster queries. The index is built immediately by scanning all documents. Returns `"created"` or `"exists"`. |
| `get_indexes` | `async fn get_indexes(&self)` | `Vec<IndexInfo>` | List all indexes defined on this database. |
| `delete_index` | `async fn delete_index(&self, name: &str)` | `Result<()>` | Delete an index by name. Returns `NotFound` if the index does not exist. |

### Example

```rust
use rouchdb::{IndexDefinition, SortField};

// Create an index on the "age" field
let result = db.create_index(IndexDefinition {
    name: String::new(), // auto-generated as "idx-age"
    fields: vec![SortField::Simple("age".into())],
    ddoc: None,
}).await?;
println!("Index: {} ({})", result.name, result.result);

// Queries on "age" now use the index instead of a full scan
let found = db.find(FindOptions {
    selector: json!({"age": {"$gte": 21}}),
    ..Default::default()
}).await?;

// List indexes
let indexes = db.get_indexes().await;

// Delete an index
db.delete_index("idx-age").await?;
```

---

## Replication

All replication methods implement the CouchDB replication protocol: checkpoint reading, changes feed, revision diff, bulk document fetch, and checkpoint saving. See the [Replication chapter](../guide/replication.md) for a conceptual overview.

| Method | Signature | Return Type | Description |
|--------|-----------|-------------|-------------|
| `replicate_to` | `async fn replicate_to(&self, target: &Database)` | `Result<ReplicationResult>` | One-shot push replication from this database to the target. Uses default options (batch size 100). |
| `replicate_from` | `async fn replicate_from(&self, source: &Database)` | `Result<ReplicationResult>` | One-shot pull replication from the source into this database. |
| `replicate_to_with_opts` | `async fn replicate_to_with_opts(&self, target: &Database, opts: ReplicationOptions)` | `Result<ReplicationResult>` | Push replication with custom `ReplicationOptions` (batch size, batches limit). |
| `replicate_to_with_events` | `async fn replicate_to_with_events(&self, target: &Database, opts: ReplicationOptions)` | `Result<(ReplicationResult, Receiver<ReplicationEvent>)>` | Push replication with event streaming. Returns the result and a channel receiver for progress events. |
| `replicate_to_live` | `fn replicate_to_live(&self, target: &Database, opts: ReplicationOptions)` | `(Receiver<ReplicationEvent>, ReplicationHandle)` | Start continuous (live) replication. Returns an event receiver and a handle to cancel. Dropping the handle also cancels. |
| `sync` | `async fn sync(&self, other: &Database)` | `Result<(ReplicationResult, ReplicationResult)>` | Bidirectional sync: pushes to `other`, then pulls from `other`. Returns a tuple of `(push_result, pull_result)`. |

### ReplicationOptions

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `batch_size` | `u64` | `100` | Number of documents to process per batch. |
| `batches_limit` | `u64` | `10` | Maximum number of batches to buffer. |
| `filter` | `Option<ReplicationFilter>` | `None` | Optional filter for selective replication. |
| `live` | `bool` | `false` | Enable continuous replication (used with `replicate_to_live`). |
| `retry` | `bool` | `false` | Automatically retry on failure (live mode). |
| `poll_interval` | `Duration` | `500ms` | How often to poll for new changes in live mode. |
| `back_off_function` | `Option<Box<dyn Fn(u32) -> Duration>>` | `None` | Custom backoff function for retries. Receives retry count, returns delay. |

### ReplicationFilter

| Variant | Description |
|---------|-------------|
| `DocIds(Vec<String>)` | Replicate only the listed document IDs. Filtering at the changes feed level (most efficient). |
| `Selector(serde_json::Value)` | Replicate documents matching a Mango selector. Evaluated after fetching documents. |
| `Custom(Box<dyn Fn(&ChangeEvent) -> bool + Send + Sync>)` | Replicate documents passing a custom predicate applied to each change event. |

### ReplicationResult

| Field | Type | Description |
|-------|------|-------------|
| `ok` | `bool` | `true` if replication completed with no errors. |
| `docs_read` | `u64` | Total number of documents read from the source changes feed. |
| `docs_written` | `u64` | Total number of documents written to the target. |
| `errors` | `Vec<String>` | List of error messages encountered during replication. |
| `last_seq` | `Seq` | The last sequence processed, used as the checkpoint for the next replication. |

### Example

```rust
let local = Database::open("local.redb", "mydb")?;
let remote = Database::http("http://localhost:5984/mydb");

// Push local changes to CouchDB
let push = local.replicate_to(&remote).await?;
println!("Pushed {} docs", push.docs_written);

// Full bidirectional sync
let (push, pull) = local.sync(&remote).await?;
```

---

## Maintenance

| Method | Signature | Return Type | Description |
|--------|-----------|-------------|-------------|
| `compact` | `async fn compact(&self)` | `Result<()>` | Compact the database: removes old revisions and cleans up unreferenced attachment data. |
| `destroy` | `async fn destroy(&self)` | `Result<()>` | Destroy the database and all its data. This is irreversible. |

---

## Accessing the Adapter

| Method | Signature | Return Type | Description |
|--------|-----------|-------------|-------------|
| `adapter` | `fn adapter(&self) -> &dyn Adapter` | `&dyn Adapter` | Get a reference to the underlying adapter. Useful when you need to call adapter-level methods not exposed on `Database` (e.g., `revs_diff`, `bulk_get`, local documents). |

### Example

```rust
let db = Database::memory("test");

// Access adapter directly for replication-level operations
let diff = db.adapter().revs_diff(rev_map).await?;
let local = db.adapter().get_local("_local/my-checkpoint").await?;
```
