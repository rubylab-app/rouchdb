# RouchDB

A local-first document database for Rust with CouchDB replication protocol support.

RouchDB is the Rust equivalent of [PouchDB](https://pouchdb.com/) — it stores JSON documents locally and syncs bidirectionally with [CouchDB](https://couchdb.apache.org/) and compatible servers. Pure Rust, no C dependencies.

[![Crates.io](https://img.shields.io/crates/v/rouchdb)](https://crates.io/crates/rouchdb)
[![Docs](https://img.shields.io/docsrs/rouchdb)](https://docs.rs/rouchdb)
[![CI](https://github.com/rubylab-app/rouchdb/actions/workflows/ci.yml/badge.svg)](https://github.com/rubylab-app/rouchdb/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## Features

- **Local-first** — works offline, syncs when connected
- **CouchDB replication protocol** — bidirectional sync with CouchDB 2.x/3.x
- **Multiple storage backends** — in-memory, persistent (redb), or remote (CouchDB HTTP)
- **Conflict resolution** — deterministic winner selection, conflicts preserved for application-level resolution
- **Mango queries** — `$eq`, `$gt`, `$regex`, `$elemMatch`, and more
- **Map/reduce views** — with built-in `_sum`, `_count`, `_stats` reducers
- **Changes feed** — one-shot, live streaming, selector/filter/doc_ids filtering
- **Attachments** — binary data stored alongside documents (memory + redb backends)
- **Design documents & views** — Rust-native ViewEngine with map/reduce
- **Plugin system** — before_write, after_write, on_destroy hooks
- **Partitioned databases** — scoped queries by ID prefix
- **CouchDB-compatible HTTP server** — browse databases with Fauxton, use any CouchDB client
- **CLI tool** — inspect, query, and modify redb databases from the terminal
- **Pure Rust** — no C dependencies (redb instead of LevelDB/SQLite)

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
rouchdb = "0.3"
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    // Create a database (in-memory, persistent, or remote)
    let db = Database::memory("mydb");
    // let db = Database::open("mydb.redb", "mydb")?;
    // let db = Database::http("http://admin:password@localhost:5984/mydb");

    // Create a document
    let result = db.put("user:alice", serde_json::json!({
        "name": "Alice",
        "email": "alice@example.com",
        "age": 30
    })).await?;
    println!("Created with rev: {}", result.rev.unwrap());

    // Read it back
    let doc = db.get("user:alice").await?;
    println!("Name: {}", doc.data["name"]);

    // Update (requires current rev)
    let rev = doc.rev.unwrap().to_string();
    db.update("user:alice", &rev, serde_json::json!({
        "name": "Alice",
        "age": 31
    })).await?;

    // Sync with another database
    let remote = Database::memory("remote");
    let (push, pull) = db.sync(&remote).await?;
    println!("Push: {} docs, Pull: {} docs", push.docs_written, pull.docs_written);

    Ok(())
}
```

## Querying

### Mango Selectors

```rust
use rouchdb::{Database, FindOptions};

let result = db.find(FindOptions {
    selector: serde_json::json!({
        "age": {"$gte": 21},
        "city": {"$in": ["NYC", "LA"]}
    }),
    sort: Some(vec![rouchdb::SortField::Simple("age".into())]),
    limit: Some(10),
    ..Default::default()
}).await?;
```

### Map/Reduce

```rust
use rouchdb::{Database, query_view, ReduceFn, ViewQueryOptions};

let result = query_view(
    db.adapter(),
    &|doc| {
        let city = doc.get("city").cloned().unwrap_or_default();
        vec![(city, serde_json::json!(1))]
    },
    Some(&ReduceFn::Count),
    ViewQueryOptions { reduce: true, group: true, ..ViewQueryOptions::new() },
).await?;
```

## Replication

Sync with CouchDB or between any two databases:

```rust
let local = Database::open("local.redb", "mydb")?;
let remote = Database::http("http://admin:password@localhost:5984/mydb");

// One-way
local.replicate_to(&remote).await?;
local.replicate_from(&remote).await?;

// Bidirectional
local.sync(&remote).await?;
```

### Live Replication

```rust
use std::time::Duration;
use rouchdb::{ReplicationOptions, ReplicationEvent};

let (mut rx, handle) = local.replicate_to_live(&remote, ReplicationOptions {
    poll_interval: Duration::from_secs(5),
    retry: true,
    ..Default::default()
});

while let Some(event) = rx.recv().await {
    match event {
        ReplicationEvent::Change { docs_read, .. } => println!("synced {docs_read} docs"),
        ReplicationEvent::Paused => println!("up to date"),
        ReplicationEvent::Error(msg) => eprintln!("error: {msg}"),
        _ => {}
    }
}

handle.cancel();
```

## Storage Backends

| Backend | Constructor | Use Case |
|---------|------------|----------|
| **Memory** | `Database::memory("name")` | Testing, ephemeral data |
| **Redb** | `Database::open("path.redb", "name")` | Persistent local storage |
| **HTTP** | `Database::http("http://...")` | Remote CouchDB |

All backends implement the same `Adapter` trait — swap storage without changing application code.

## HTTP Server & Fauxton

RouchDB includes a CouchDB-compatible HTTP server with the Fauxton web dashboard:

```bash
# Install the server
cargo install --path crates/rouchdb-server

# Download Fauxton (optional, requires Node.js >= 10)
bash scripts/download-fauxton.sh

# Start the server
rouchdb-server mydb.redb --port 5984

# Open Fauxton in your browser
open http://localhost:5984/_utils/
```

The server exposes 25+ CouchDB-compatible REST endpoints — documents, queries, changes feed, attachments, security, design docs, Mango indexes, and more — so any CouchDB client (Fauxton, PouchDB, curl) can connect to it.

Options:

```
rouchdb-server <path.redb> [OPTIONS]

Options:
  -p, --port <PORT>        Port to listen on [default: 5984]
      --host <HOST>        Host to bind to [default: 127.0.0.1]
      --db-name <NAME>     Database name [default: filename without extension]
```

## CLI Tool

A command-line tool for inspecting, querying, and modifying redb database files:

```bash
cargo install --path crates/rouchdb-cli
```

### Reading

```bash
rouchdb info mydb.redb                    # Database info
rouchdb get mydb.redb user:alice          # Get document by ID
rouchdb all-docs mydb.redb --include-docs # List all documents
rouchdb find mydb.redb --selector '{"age": {"$gte": 30}}'  # Mango query
rouchdb changes mydb.redb --include-docs  # Changes feed
rouchdb dump mydb.redb --pretty           # Export all docs as JSON
```

### Writing

```bash
rouchdb put mydb.redb user:alice '{"name":"Alice","age":30}'              # Create
rouchdb put mydb.redb user:alice '{"name":"Alice","age":31}' --rev 1-abc  # Update
rouchdb put mydb.redb user:alice '{"name":"Alice","age":32}' --force      # Upsert (auto-fetches rev)
rouchdb post mydb.redb '{"name":"Bob","age":25}'                       # Auto-ID
rouchdb delete mydb.redb user:alice --rev 2-def                        # Delete
rouchdb import mydb.redb docs.json                                     # Bulk import
```

### Operations

```bash
rouchdb replicate mydb.redb http://admin:password@localhost:5984/mydb  # Sync
rouchdb compact mydb.redb                                              # Compact
```

Add `--pretty` (or `-p`) to any command for formatted JSON output.

## Crate Structure

RouchDB is a workspace of 11 crates:

| Crate | Description |
|-------|-------------|
| `rouchdb` | Umbrella crate with `Database` API |
| `rouchdb-core` | Traits, types, revision tree, merge algorithm, collation |
| `rouchdb-adapter-memory` | In-memory storage adapter |
| `rouchdb-adapter-redb` | Persistent storage via [redb](https://crates.io/crates/redb) |
| `rouchdb-adapter-http` | CouchDB HTTP client adapter with cookie auth |
| `rouchdb-changes` | Changes feed and live streaming |
| `rouchdb-replication` | CouchDB replication protocol |
| `rouchdb-query` | Mango queries and map/reduce views |
| `rouchdb-views` | Design documents and persistent view engine |
| `rouchdb-server` | CouchDB-compatible HTTP server with Fauxton |
| `rouchdb-cli` | Command-line tool for database inspection and CRUD |

## Documentation

- [**Book**](https://rubylabapp.github.io/rouchdb/) — guides, reference, and architecture docs
- [**API Docs**](https://docs.rs/rouchdb) — generated Rust docs

## Development

```bash
# Run tests
cargo test

# Lint
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# Integration tests (require CouchDB)
docker compose up -d
cargo test -p rouchdb --test '*' -- --ignored
```

## License

MIT
