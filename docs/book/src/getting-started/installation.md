# Installation

## Full Package

Add RouchDB to your project with all features:

```toml
[dependencies]
rouchdb = "0.3"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

This gives you local storage (redb), HTTP client, replication, queries, and the high-level `Database` API.

## Minimal Setup

If you only need local storage without replication or HTTP:

```toml
[dependencies]
rouchdb-core = "0.3"
rouchdb-adapter-redb = "0.3"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Individual Crates

Pick exactly what you need:

| Crate | What it adds |
|-------|-------------|
| `rouchdb-core` | Types, traits, revision tree, collation, errors |
| `rouchdb-adapter-memory` | In-memory adapter (testing, ephemeral data) |
| `rouchdb-adapter-redb` | Persistent local storage via redb |
| `rouchdb-adapter-http` | CouchDB HTTP client adapter |
| `rouchdb-changes` | Changes feed (one-shot and live streaming) |
| `rouchdb-replication` | CouchDB replication protocol |
| `rouchdb-query` | Mango selectors and map/reduce views |
| `rouchdb-views` | Design documents and persistent view engine |
| `rouchdb-server` | CouchDB-compatible HTTP server with Fauxton |
| `rouchdb-cli` | CLI tool for inspecting databases |
| `rouchdb` | Umbrella crate — re-exports everything above |

## CLI Tool

RouchDB includes a command-line tool for inspecting and querying redb database files. Install it from source:

```bash
cargo install --path crates/rouchdb-cli
```

This installs the `rouchdb` binary. Usage examples:

```bash
# Show database info
rouchdb info mydb.redb

# Get a document by ID
rouchdb get mydb.redb user:alice

# List all documents with their bodies
rouchdb all-docs mydb.redb --include-docs

# Query with a Mango selector
rouchdb find mydb.redb --selector '{"age": {"$gte": 30}}'

# View the changes feed
rouchdb changes mydb.redb --include-docs

# Export all documents as JSON
rouchdb dump mydb.redb --pretty

# Create or update a document
rouchdb put mydb.redb user:alice '{"name":"Alice","age":30}'
rouchdb put mydb.redb user:alice '{"name":"Alice","age":31}' --rev 1-abc

# Upsert — auto-fetches the current rev, creates if missing
rouchdb put mydb.redb user:alice '{"name":"Alice","age":32}' --force

# Create a document with auto-generated ID
rouchdb post mydb.redb '{"name":"Bob","age":25}'

# Delete a document
rouchdb delete mydb.redb user:alice --rev 2-def

# Import documents from a JSON file
rouchdb import mydb.redb docs.json

# Replicate to CouchDB
rouchdb replicate mydb.redb http://admin:password@localhost:5984/mydb

# Compact the database
rouchdb compact mydb.redb
```

Add `--pretty` (or `-p`) to any command for formatted JSON output.

## HTTP Server

RouchDB includes a CouchDB-compatible HTTP server that lets you browse databases with the Fauxton web UI or connect any CouchDB client. Install it from source:

```bash
cargo install --path crates/rouchdb-server
```

Download Fauxton (optional, for the web dashboard):

```bash
bash scripts/download-fauxton.sh
```

Start the server:

```bash
# Serve a redb database file
rouchdb-server mydb.redb --port 5984

# Open Fauxton in your browser
open http://localhost:5984/_utils/
```

The server exposes CouchDB-compatible REST endpoints — documents, queries, changes feed, attachments, security, design docs, and more — so tools like PouchDB, curl, or any CouchDB client library can connect to it.

Options:

```bash
rouchdb-server <path.redb> [OPTIONS]

Options:
  -p, --port <PORT>        Port to listen on [default: 5984]
      --host <HOST>        Host to bind to [default: 127.0.0.1]
      --db-name <NAME>     Database name [default: filename without extension]
```

## Async Runtime

RouchDB is built on [Tokio](https://tokio.rs/). All database operations are `async` and require a Tokio runtime:

```rust
#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let db = rouchdb::Database::memory("mydb");
    // ... your code here
    Ok(())
}
```

## Verifying the Installation

```rust
use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let db = Database::memory("test");

    let result = db.put("hello", serde_json::json!({"msg": "it works!"})).await?;
    assert!(result.ok);

    let doc = db.get("hello").await?;
    println!("{}", doc.data["msg"]); // "it works!"

    Ok(())
}
```

If this compiles and prints `"it works!"`, you're all set. Head to the [Quickstart](./quickstart.md).
