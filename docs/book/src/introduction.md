# RouchDB

**A local-first document database for Rust with CouchDB replication protocol support.**

RouchDB is the Rust equivalent of [PouchDB](https://pouchdb.com/) — it gives you a local JSON document store that syncs bidirectionally with [Apache CouchDB](https://couchdb.apache.org/) and compatible servers.

## Why RouchDB?

- **No equivalent exists in Rust.** Crates like `couch_rs` provide CouchDB HTTP clients, but none offer local storage with replication. RouchDB fills this gap.
- **Offline-first by design.** Your app works without a network connection. When connectivity returns, RouchDB syncs changes automatically.
- **Full CouchDB compatibility.** Implements the CouchDB replication protocol, revision tree model, and Mango query language.

## Features

- **CRUD operations** — `put`, `get`, `update`, `remove`, `bulk_docs`, `all_docs`
- **Mango queries** — `$eq`, `$gt`, `$in`, `$regex`, `$or`, and 15+ operators
- **Map/reduce views** — Rust closures with built-in `Sum`, `Count`, `Stats` reduces
- **Changes feed** — one-shot and live streaming of document mutations
- **Replication** — push, pull, and bidirectional sync with CouchDB
- **Conflict resolution** — deterministic winner algorithm, conflict detection utilities
- **CouchDB-compatible HTTP server** — browse databases with Fauxton, connect any CouchDB client
- **Pluggable storage** — in-memory, persistent (redb), or remote (HTTP)
- **Pure Rust** — no C dependencies, compiles everywhere Rust does

## Target Use Cases

- Desktop apps with [Tauri](https://tauri.app/) that need offline sync
- CLI tools that store data locally and sync to a server
- Backend services that replicate between CouchDB instances
- Any Rust application that needs a local document database

## Quick Example

```rust
use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let db = Database::memory("mydb");

    // Create a document
    let result = db.put("user:alice", serde_json::json!({
        "name": "Alice",
        "age": 30
    })).await?;

    // Read it back
    let doc = db.get("user:alice").await?;
    println!("{}: {}", doc.id, doc.data);

    Ok(())
}
```

Ready to start? Head to [Installation](./getting-started/installation.md).
