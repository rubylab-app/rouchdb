# Partitioned Databases

Partitions let you scope queries to a subset of documents that share a common ID prefix. This is useful for multi-tenant applications or logical grouping.

## How It Works

Documents are partitioned by their ID prefix. A document with ID `"tenant-a:invoice-001"` belongs to the `"tenant-a"` partition. All queries on a partition are automatically scoped to documents whose ID starts with `"{partition}:"`.

## Creating a Partition

```rust
use rouchdb::Database;

let db = Database::memory("mydb");

// Insert documents with partition prefixes
db.put("tenant-a:doc1", serde_json::json!({"name": "Alice"})).await?;
db.put("tenant-a:doc2", serde_json::json!({"name": "Bob"})).await?;
db.put("tenant-b:doc1", serde_json::json!({"name": "Charlie"})).await?;

// Create a partition view
let partition = db.partition("tenant-a");
```

## Partition Operations

### Get

Retrieve a document within the partition. The partition prefix is added automatically:

```rust
let partition = db.partition("tenant-a");

// These are equivalent:
let doc = partition.get("doc1").await?;
let doc = partition.get("tenant-a:doc1").await?;
```

### Put

Create or update a document in the partition:

```rust
let partition = db.partition("tenant-a");
partition.put("doc3", serde_json::json!({"name": "Diana"})).await?;
// Creates document with ID "tenant-a:doc3"
```

### All Docs

Query all documents in the partition:

```rust
use rouchdb::AllDocsOptions;

let partition = db.partition("tenant-a");
let result = partition.all_docs(AllDocsOptions {
    include_docs: true,
    ..AllDocsOptions::new()
}).await?;

// Only returns tenant-a documents
for row in &result.rows {
    assert!(row.id.starts_with("tenant-a:"));
}
```

### Find (Mango Query)

Run a Mango query scoped to the partition:

```rust
use rouchdb::FindOptions;

let partition = db.partition("tenant-a");
let result = partition.find(FindOptions {
    selector: serde_json::json!({"name": {"$regex": "^A"}}),
    ..Default::default()
}).await?;

// Only searches within tenant-a documents
```

## Partition Isolation

Partitions are enforced at query time. Documents from other partitions are never included in results:

```rust
let a = db.partition("tenant-a");
let b = db.partition("tenant-b");

let a_docs = a.all_docs(AllDocsOptions::new()).await?;
let b_docs = b.all_docs(AllDocsOptions::new()).await?;

// No overlap between partitions
```
