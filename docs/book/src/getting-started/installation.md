# Installation

## Full Package

Add RouchDB to your project with all features:

```toml
[dependencies]
rouchdb = "0.4"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

This gives you local storage (redb), HTTP client, replication, queries, and the high-level `Database` API.

RouchDB requires Rust 1.88 or newer.

## TLS Backend

HTTPS connections to CouchDB (`Database::http("https://...")`) use [rustls](https://github.com/rustls/rustls) with the bundled Mozilla root certificates by default, so no OpenSSL or other system library is needed. Choose another backend with Cargo features:

| Feature | TLS stack | Trusted roots |
|---------|-----------|---------------|
| `rustls-tls` (default) | rustls | Bundled Mozilla roots |
| `rustls-tls-native-roots` | rustls | The operating system's certificate store (e.g. a private CA) |
| `native-tls` | Platform stack (OpenSSL on Linux) | The operating system's certificate store |

```toml
[dependencies]
rouchdb = { version = "0.4", default-features = false, features = ["native-tls"] }
```

With `default-features = false` and no TLS feature, only plain `http://` URLs work, and the build has no C code at all (rustls' default crypto provider, ring, compiles a small amount of bundled C and assembly). The same features exist on `rouchdb-adapter-http`. They are new after 0.4.0; version 0.4.0 always uses native-tls.

### Exact numbers

By default JSON numbers are `serde_json` numbers: integers that fit `i64`/`u64` are exact, anything else goes through `f64` (so `18446744073709551616` is stored as `1.8446744073709552e19` and `1.50` as `1.5`). The opt-in `arbitrary-precision` feature keeps every number exactly as written, through the memory and redb adapters, replication and the HTTP adapter (CouchDB itself keeps integers of any size exact and rounds decimals to doubles):

```sh
cargo add rouchdb --features arbitrary-precision
```

It enables serde_json's `arbitrary_precision` for the whole build, which also changes `serde_json::Number` for your own code (comparisons are textual: `1.0` and `1.00` are different numbers), and revision ids of documents with such numbers differ from those computed without the feature. See [Differences from CouchDB](../reference/differences.md#numbers).

## Minimal Setup

If you only need local storage without replication or HTTP:

```toml
[dependencies]
rouchdb-core = "0.4"
rouchdb-adapter-redb = "0.4"
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
  -p, --port <PORT>                Port to listen on [default: 5984]
      --host <HOST>                Host to bind to [default: 127.0.0.1]
      --db-name <NAME>             Database name [default: filename without extension]
      --admin <USER:PASSWORD>      Require admin credentials [env: ROUCHDB_ADMIN]
      --cors-origin <ORIGIN>       Allow CORS from this origin (repeatable) [env: ROUCHDB_CORS_ORIGINS]
      --max-request-size <BYTES>   Largest accepted request body [default: 67108864]
      --session-timeout <SECONDS>  Idle lifetime of a _session cookie [default: 600]
```

By default the server listens on `127.0.0.1` with CORS disabled and no
authentication. Set `ROUCHDB_ADMIN=user:password` to require credentials
(HTTP Basic auth or a `_session` cookie, which expires after
`--session-timeout` seconds without use), and allow browser apps on other
origins explicitly with `--cors-origin`.

The server serves a single database. `DELETE /{db}` destroys its data and the
database answers 404 until `PUT /{db}` creates it again (a restart also brings
it back, empty).

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
