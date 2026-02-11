# Replication

Replication is what makes RouchDB a local-first database. It implements the CouchDB replication protocol, allowing bidirectional sync between any two databases -- local to local, local to remote CouchDB, or even remote to remote.

## Quick Start

```rust
use rouchdb::Database;
use serde_json::json;

let local = Database::open("data/myapp.redb", "myapp")?;
let remote = Database::http("http://localhost:5984/myapp");

// Push local changes to CouchDB
local.replicate_to(&remote).await?;

// Pull remote changes to local
local.replicate_from(&remote).await?;

// Or do both directions at once
let (push_result, pull_result) = local.sync(&remote).await?;
```

## Setting Up CouchDB with Docker

A minimal `docker-compose.yml` for local development:

```yaml
version: "3"
services:
  couchdb:
    image: couchdb:3
    ports:
      - "5984:5984"
    environment:
      COUCHDB_USER: admin
      COUCHDB_PASSWORD: password
    volumes:
      - couchdata:/opt/couchdb/data

volumes:
  couchdata:
```

Start it and create a database:

```bash
docker compose up -d

# Create the database
curl -X PUT http://admin:password@localhost:5984/myapp
```

Then connect from RouchDB:

```rust
let remote = Database::http("http://admin:password@localhost:5984/myapp");
```

## Replication Methods

### replicate_to

Push documents from this database to a target.

```rust
let result = local.replicate_to(&remote).await?;
println!("Pushed {} docs", result.docs_written);
```

### replicate_from

Pull documents from a source into this database.

```rust
let result = local.replicate_from(&remote).await?;
println!("Pulled {} docs", result.docs_written);
```

### sync

Bidirectional sync: pushes first, then pulls. Returns both results as a tuple.

```rust
let (push, pull) = local.sync(&remote).await?;

println!("Push: {} written, Pull: {} written",
    push.docs_written, pull.docs_written);
```

### replicate_to_with_opts

Push with custom replication options.

```rust
use rouchdb::ReplicationOptions;

let result = local.replicate_to_with_opts(&remote, ReplicationOptions {
    batch_size: 50,
    ..Default::default()
}).await?;
```

## ReplicationOptions

```rust
use rouchdb::ReplicationOptions;

let opts = ReplicationOptions {
    batch_size: 100,                                   // documents per batch (default: 100)
    batches_limit: 10,                                 // max batches to buffer (default: 10)
    filter: None,                                      // optional filter (default: None)
    live: false,                                       // continuous mode (default: false)
    retry: false,                                      // auto-retry on failure (default: false)
    poll_interval: std::time::Duration::from_millis(500), // live mode poll interval
    back_off_function: None,                           // custom backoff for retries
    ..Default::default()
};
```

| Field | Default | Description |
|-------|---------|-------------|
| `batch_size` | 100 | Number of documents to process in each replication batch. Smaller values mean more frequent checkpoints. |
| `batches_limit` | 10 | Maximum number of batches to buffer. Controls memory usage for large replications. |
| `filter` | `None` | Optional `ReplicationFilter` for selective replication. See [Filtered Replication](#filtered-replication). |
| `since` | `None` | Override the starting sequence instead of reading from checkpoint. Useful for replaying changes from a known point. |
| `checkpoint` | `true` | Set to `false` to disable checkpoint saving. Each replication will start from the beginning (or `since`). |
| `live` | `false` | Enable continuous replication that keeps running and picks up new changes. |
| `retry` | `false` | Automatically retry on network or transient errors (live mode). |
| `poll_interval` | 500ms | How frequently to poll for new changes in live mode. |
| `back_off_function` | `None` | Custom backoff function for retries. Receives retry count, returns delay. |

## Filtered Replication

You can replicate a subset of documents using `ReplicationFilter`. Three filter types are available:

### Filter by Document IDs

Replicate only specific documents by their IDs. This is the most efficient filter -- it pushes the filtering down to the changes feed.

```rust
use rouchdb::{ReplicationOptions, ReplicationFilter};

let result = local.replicate_to_with_opts(&remote, ReplicationOptions {
    filter: Some(ReplicationFilter::DocIds(vec![
        "user:alice".into(),
        "user:bob".into(),
    ])),
    ..Default::default()
}).await?;
```

### Filter by Mango Selector

Replicate documents matching a Mango query selector. The selector is evaluated against each document's data after fetching.

```rust
use rouchdb::{ReplicationOptions, ReplicationFilter};

let result = local.replicate_to_with_opts(&remote, ReplicationOptions {
    filter: Some(ReplicationFilter::Selector(serde_json::json!({
        "type": "invoice",
        "status": "pending"
    }))),
    ..Default::default()
}).await?;
```

### Filter by Custom Closure

Pass a Rust closure that receives each `ChangeEvent` and returns `true` to replicate or `false` to skip.

```rust
use rouchdb::{ReplicationOptions, ReplicationFilter};

let result = local.replicate_to_with_opts(&remote, ReplicationOptions {
    filter: Some(ReplicationFilter::Custom(std::sync::Arc::new(|change| {
        change.id.starts_with("public:")
    }))),
    ..Default::default()
}).await?;
```

**Note:** Checkpoints advance past all processed changes regardless of filtering. This means re-running filtered replication won't re-evaluate previously seen changes.

## How the Replication Protocol Works

RouchDB implements the standard CouchDB replication protocol. Each replication run follows these steps:

1. **Read checkpoint** -- Load the last successfully replicated sequence from the local document store. This allows replication to resume where it left off.

2. **Fetch changes** -- Query the source's changes feed starting from the checkpoint sequence, limited to `batch_size` changes per request.

3. **Compute revs_diff** -- Send the changed document IDs and their revisions to the target. The target responds with which revisions it is missing, avoiding redundant transfers.

4. **Fetch missing documents** -- Use `bulk_get` to retrieve only the documents and revisions the target does not have.

5. **Write to target** -- Write the missing documents to the target using `bulk_docs` with `new_edits: false` (replication mode), which preserves the original revision IDs and merges them into the target's revision trees.

6. **Save checkpoint** -- Persist the last replicated sequence so the next run can start from where this one ended.

Steps 2-6 repeat in a loop until no more changes remain.

## ReplicationResult

Every replication call returns a `ReplicationResult`:

```rust
use rouchdb::ReplicationResult;

let result = local.replicate_to(&remote).await?;

if result.ok {
    println!("Replication succeeded");
} else {
    println!("Replication had errors:");
    for err in &result.errors {
        println!("  - {}", err);
    }
}

println!("Documents read:    {}", result.docs_read);
println!("Documents written: {}", result.docs_written);
println!("Last sequence:     {}", result.last_seq);
```

| Field | Type | Description |
|-------|------|-------------|
| `ok` | `bool` | `true` if no errors occurred. |
| `docs_read` | `u64` | Number of change events read from the source. |
| `docs_written` | `u64` | Number of documents written to the target. |
| `errors` | `Vec<String>` | Descriptions of any errors during replication. |
| `last_seq` | `Seq` | The source sequence up to which replication completed. |

Note that `docs_read` may be greater than `docs_written` when the target already has some of the documents (incremental replication).

## Incremental Replication

Replication is incremental by default. Checkpoints are stored as local documents (prefixed with `_local/`) that are not themselves replicated. After an initial full sync, subsequent calls only transfer new changes:

```rust
// First run: syncs everything
let r1 = local.replicate_to(&remote).await?;
println!("Initial: {} docs written", r1.docs_written); // e.g. 500

// Add some new documents
local.put("new_doc", json!({"data": "hello"})).await?;

// Second run: only syncs the delta
let r2 = local.replicate_to(&remote).await?;
println!("Incremental: {} docs written", r2.docs_written); // 1
```

## Replication Events

Use `replicate_to_with_events()` to receive progress events during replication:

```rust
use rouchdb::ReplicationEvent;

let (result, mut rx) = local.replicate_to_with_events(
    &remote,
    ReplicationOptions::default(),
).await?;

// Drain events after replication completes
while let Ok(event) = rx.try_recv() {
    match event {
        ReplicationEvent::Active => println!("Replication started"),
        ReplicationEvent::Change { docs_read } => {
            println!("Progress: {} docs read", docs_read);
        }
        ReplicationEvent::Complete(result) => {
            println!("Done: {} written", result.docs_written);
        }
        ReplicationEvent::Error(msg) => println!("Error: {}", msg),
        ReplicationEvent::Paused => println!("Waiting for changes..."),
    }
}
```

### Event Variants

| Variant | Description |
|---------|-------------|
| `Active` | Replication has started or resumed processing. |
| `Change { docs_read }` | A batch of changes was processed. |
| `Paused` | Waiting for more changes (live mode). |
| `Complete(ReplicationResult)` | Replication finished (one-shot or one cycle in live mode). |
| `Error(String)` | An error occurred during replication. |

## Live (Continuous) Replication

Live replication keeps running in the background, continuously polling for new changes and replicating them. This is the equivalent of PouchDB's `{ live: true }` option.

```rust
use rouchdb::{ReplicationOptions, ReplicationEvent};

let (mut rx, handle) = local.replicate_to_live(&remote, ReplicationOptions {
    live: true,
    poll_interval: std::time::Duration::from_millis(500),
    retry: true,
    ..Default::default()
});

// Process events in a loop
tokio::spawn(async move {
    while let Some(event) = rx.recv().await {
        match event {
            ReplicationEvent::Complete(r) => {
                println!("Batch done: {} docs written", r.docs_written);
            }
            ReplicationEvent::Paused => {
                println!("Up to date, waiting for new changes...");
            }
            ReplicationEvent::Error(msg) => {
                eprintln!("Replication error: {}", msg);
            }
            _ => {}
        }
    }
});

// ... later, when you want to stop:
handle.cancel();
```

### ReplicationHandle

The `ReplicationHandle` returned by `replicate_to_live()` controls the background replication:

- **`handle.cancel()`** -- Stops the replication gracefully.
- **Dropping the handle** also cancels the replication (via `Drop` implementation).

### Retry and Backoff

When `retry: true` is set, live replication will automatically retry after transient errors. You can customize the backoff strategy:

```rust
let (rx, handle) = local.replicate_to_live(&remote, ReplicationOptions {
    live: true,
    retry: true,
    back_off_function: Some(Box::new(|retry_count| {
        // Exponential backoff: 1s, 2s, 4s, 8s, max 30s
        let delay = std::cmp::min(1000 * 2u64.pow(retry_count), 30_000);
        std::time::Duration::from_millis(delay)
    })),
    ..Default::default()
});
```

## Complete Example: Local-to-CouchDB Sync

```rust
use rouchdb::{Database, ReplicationOptions};
use serde_json::json;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    // Open persistent local database
    let local = Database::open("data/todos.redb", "todos")?;

    // Connect to CouchDB
    let remote = Database::http("http://admin:password@localhost:5984/todos");

    // Create some local documents
    local.put("todo:1", json!({
        "title": "Buy groceries",
        "done": false
    })).await?;

    local.put("todo:2", json!({
        "title": "Write documentation",
        "done": true
    })).await?;

    // Push to CouchDB with custom batch size
    let push = local.replicate_to_with_opts(&remote, ReplicationOptions {
        batch_size: 50,
        ..Default::default()
    }).await?;

    println!("Push complete: {} docs written", push.docs_written);

    // Pull any changes others made on CouchDB
    let pull = local.replicate_from(&remote).await?;
    println!("Pull complete: {} docs written", pull.docs_written);

    // Check local state
    let info = local.info().await?;
    println!("Local database: {} docs, seq {}",
        info.doc_count, info.update_seq);

    Ok(())
}
```

## Local-to-Local Replication

Replication works between any two adapters, not just local and remote. This is useful for backup, migration, or testing:

```rust
let db_a = Database::memory("a");
let db_b = Database::memory("b");

db_a.put("doc1", json!({"from": "a"})).await?;
db_b.put("doc2", json!({"from": "b"})).await?;

// Sync both directions
let (push, pull) = db_a.sync(&db_b).await?;

// Both databases now have both documents
assert_eq!(db_a.info().await?.doc_count, 2);
assert_eq!(db_b.info().await?.doc_count, 2);
```
