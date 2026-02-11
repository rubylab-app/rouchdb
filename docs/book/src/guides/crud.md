# CRUD Operations

RouchDB provides a familiar document database API for creating, reading, updating, and deleting JSON documents. Every document has an `_id` (string identifier) and a `_rev` (revision string) that tracks its edit history.

## Creating a Database

```rust
use rouchdb::Database;

// In-memory (data lost when dropped; great for tests)
let db = Database::memory("mydb");

// Persistent storage backed by redb
let db = Database::open("path/to/mydb.redb", "mydb")?;

// Remote CouchDB instance
let db = Database::http("http://localhost:5984/mydb");
```

## Put (Create)

Use `put()` to create a new document with a given ID. The data is any `serde_json::Value`.

```rust
use serde_json::json;

let result = db.put("user:alice", json!({
    "name": "Alice",
    "email": "alice@example.com",
    "age": 30
})).await?;

assert!(result.ok);
assert_eq!(result.id, "user:alice");

// The new revision string (e.g. "1-a3f2b...")
let rev = result.rev.unwrap();
```

The returned `DocResult` contains:
- `ok` -- whether the write succeeded.
- `id` -- the document ID.
- `rev` -- the new revision string (if successful).
- `error` / `reason` -- error details (if failed).

## Get (Read)

Retrieve a document by its ID.

```rust
let doc = db.get("user:alice").await?;

println!("Name: {}", doc.data["name"]);
println!("Rev:  {}", doc.rev.as_ref().unwrap());
```

The `Document` struct contains:
- `id` -- the document ID.
- `rev` -- the current `Revision` (pos + hash).
- `deleted` -- whether this document is deleted.
- `data` -- the JSON body as `serde_json::Value`.
- `attachments` -- a map of `AttachmentMeta` entries.

### Get with Options

Use `get_with_opts()` when you need a specific revision or conflict information.

```rust
use rouchdb::GetOptions;

let doc = db.get_with_opts("user:alice", GetOptions {
    rev: Some("1-a3f2b...".to_string()),
    conflicts: true,
    ..Default::default()
}).await?;
```

`GetOptions` fields:
- `rev` -- fetch a specific revision instead of the winner.
- `conflicts` -- include conflicting revision IDs.
- `open_revs` -- return all open (leaf) revisions.
- `revs` -- include the full revision history chain.

## Update

To update a document you must supply the current revision. This prevents lost updates when multiple writers are active.

```rust
let r1 = db.put("user:alice", json!({"name": "Alice", "age": 30})).await?;
let rev = r1.rev.unwrap();

let r2 = db.update("user:alice", &rev, json!({
    "name": "Alice",
    "age": 31,
    "email": "alice@newdomain.com"
})).await?;

assert!(r2.ok);
// r2.rev is now "2-..."
```

If you pass a stale revision, RouchDB returns `RouchError::Conflict`.

## Remove (Delete)

Deletion in CouchDB-compatible databases is a soft delete: the document is marked with `_deleted: true` and a new revision is created.

```rust
let doc = db.get("user:alice").await?;
let rev = doc.rev.unwrap().to_string();

let result = db.remove("user:alice", &rev).await?;
assert!(result.ok);
```

After deletion, `db.get("user:alice")` returns `RouchError::NotFound`. The document still participates in replication so that deletions propagate to other replicas.

## Bulk Docs

Write multiple documents in a single atomic operation. This is more efficient than individual puts, and it is the only way to write documents with explicit revision control.

```rust
use rouchdb::{Document, BulkDocsOptions};
use std::collections::HashMap;

let docs = vec![
    Document {
        id: "user:bob".into(),
        rev: None,
        deleted: false,
        data: json!({"name": "Bob"}),
        attachments: HashMap::new(),
    },
    Document {
        id: "user:carol".into(),
        rev: None,
        deleted: false,
        data: json!({"name": "Carol"}),
        attachments: HashMap::new(),
    },
];

let results = db.bulk_docs(docs, BulkDocsOptions::new()).await?;

for r in &results {
    println!("{}: ok={}", r.id, r.ok);
}
```

`BulkDocsOptions` has one key field:
- `new_edits` (default `true`) -- when true, the adapter generates new revision IDs and checks for conflicts. Set to `false` via `BulkDocsOptions::replication()` during replication, where revisions are accepted as-is.

## All Docs

Query all documents in the database, optionally filtered by key range.

```rust
use rouchdb::AllDocsOptions;

let response = db.all_docs(AllDocsOptions {
    include_docs: true,
    start_key: Some("user:a".into()),
    end_key: Some("user:d".into()),
    limit: Some(10),
    ..AllDocsOptions::new()
}).await?;

for row in &response.rows {
    println!("{} rev={}", row.id, row.value.rev);
    if let Some(ref doc) = row.doc {
        println!("  data: {}", doc);
    }
}
```

`AllDocsOptions` fields:
- `start_key` / `end_key` -- define the key range (string-sorted).
- `key` -- fetch a single key.
- `keys` -- fetch a specific set of keys.
- `include_docs` -- embed full document bodies in results.
- `conflicts` -- include conflict information for each document.
- `update_seq` -- include the database update sequence in the response.
- `descending` -- reverse the sort order.
- `skip` -- number of rows to skip.
- `limit` -- maximum number of rows.
- `inclusive_end` -- whether to include `end_key` (default `true`).

The response is an `AllDocsResponse` with `total_rows`, `offset`, and a `rows` vector of `AllDocsRow`.

## Error Handling

RouchDB uses the `RouchError` enum for all errors. The most common ones in CRUD operations:

```rust
use rouchdb::RouchError;

match db.get("nonexistent").await {
    Ok(doc) => println!("Found: {}", doc.id),
    Err(RouchError::NotFound(msg)) => {
        println!("Document not found: {}", msg);
    }
    Err(e) => println!("Other error: {}", e),
}

// Conflict when updating with a stale revision
match db.update("user:alice", "1-stale", json!({})).await {
    Err(RouchError::Conflict) => {
        println!("Conflict! Re-read and retry.");
    }
    _ => {}
}
```

Key error variants:
- `RouchError::NotFound(String)` -- document or resource does not exist.
- `RouchError::Conflict` -- document update conflict (stale revision).
- `RouchError::BadRequest(String)` -- malformed input.
- `RouchError::InvalidRev(String)` -- unparseable revision string.
- `RouchError::MissingId` -- document ID was not provided.
- `RouchError::Unauthorized` / `RouchError::Forbidden(String)` -- access denied.
- `RouchError::DatabaseError(String)` -- storage-level errors.

### Idiomatic Update-Retry Pattern

Because conflicts are expected in a multi-replica system, a common pattern is to re-read and retry:

```rust
loop {
    let doc = db.get("counter").await?;
    let rev = doc.rev.unwrap().to_string();
    let mut count = doc.data["count"].as_u64().unwrap_or(0);
    count += 1;

    match db.update("counter", &rev, json!({"count": count})).await {
        Ok(_) => break,
        Err(RouchError::Conflict) => continue, // retry
        Err(e) => return Err(e),
    }
}
```
