# RouchDB

A local-first document database for Rust with CouchDB replication protocol support.

RouchDB is the Rust equivalent of [PouchDB](https://pouchdb.com/) — it stores JSON documents locally and syncs bidirectionally with [CouchDB](https://couchdb.apache.org/) and compatible servers. No system libraries required: storage is pure Rust (redb) and HTTPS uses rustls by default, not OpenSSL.

[![Crates.io](https://img.shields.io/crates/v/rouchdb)](https://crates.io/crates/rouchdb)
[![Docs](https://img.shields.io/docsrs/rouchdb)](https://docs.rs/rouchdb)
[![CI](https://github.com/rubylab-app/rouchdb/actions/workflows/ci.yml/badge.svg)](https://github.com/rubylab-app/rouchdb/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> **Upgrading from 0.4?** 0.5 does not open `.redb` files written by 0.4 until you upgrade them once, with `rouchdb migrate app.redb` (which writes a verified backup first; `--dry-run` only reads the file) or `Database::open_with` + `UpgradePolicy::WithBackup`, with about three times the file size of free disk space; afterwards 0.4 cannot open them. The first `compact()` after the upgrade deletes old revision bodies. Read the [migration guide](https://github.com/rubylab-app/rouchdb/blob/main/docs/book/src/upgrading/0.4-to-0.5.md#redb-files-upgrade-once-explicitly).

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
- **No system libraries** — pure-Rust storage (redb instead of LevelDB/SQLite) and rustls for HTTPS by default

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
rouchdb = "0.5"
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Requires Rust 1.88 or newer.

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

### TLS

HTTPS to CouchDB uses [rustls](https://github.com/rustls/rustls) by default. To use the platform TLS stack instead (OpenSSL on Linux), or to trust the OS certificate store, pick a different TLS feature:

```toml
rouchdb = { version = "0.5", default-features = false, features = ["native-tls"] }
# or: features = ["rustls-tls-native-roots"]
```

With no TLS feature only plain `http://` CouchDB URLs work. These features are new in 0.5.0 (see the [changelog](CHANGELOG.md)); 0.4.x always uses native-tls.

## Querying

### Mango Selectors

```rust
use rouchdb::{Database, FindOptions};

async fn adults_in_nyc_or_la(db: &Database) -> rouchdb::Result<()> {
    let result = db.find(FindOptions {
        selector: serde_json::json!({
            "age": {"$gte": 21},
            "city": {"$in": ["NYC", "LA"]}
        }),
        sort: Some(vec![rouchdb::SortField::Simple("age".into())]),
        limit: Some(10),
        ..Default::default()
    }).await?;
    println!("{} matches", result.docs.len());
    Ok(())
}
```

### Map/Reduce

```rust
use rouchdb::{Database, query_view, ReduceFn, ViewQueryOptions};

async fn count_by_city(db: &Database) -> rouchdb::Result<()> {
    let result = query_view(
        db.adapter(),
        &|doc| {
            let city = doc.get("city").cloned().unwrap_or_default();
            vec![(city, serde_json::json!(1))]
        },
        Some(&ReduceFn::Count),
        ViewQueryOptions { reduce: true, group: true, ..ViewQueryOptions::new() },
    ).await?;
    println!("{} cities", result.rows.len());
    Ok(())
}
```

## Replication

Sync with CouchDB or between any two databases:

```rust
use rouchdb::Database;

async fn sync_with_couchdb() -> rouchdb::Result<()> {
    let local = Database::open("local.redb", "mydb")?;
    let remote = Database::http("http://admin:password@localhost:5984/mydb");

    // One-way
    local.replicate_to(&remote).await?;
    local.replicate_from(&remote).await?;

    // Bidirectional
    local.sync(&remote).await?;
    Ok(())
}
```

### Live Replication

```rust
use std::time::Duration;
use rouchdb::{Database, ReplicationEvent, ReplicationOptions};

async fn live_push(local: &Database, remote: &Database) {
    let (mut rx, handle) = local.replicate_to_live(remote, ReplicationOptions {
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
}
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

```text
rouchdb-server <path.redb> [OPTIONS]

Options:
  -p, --port <PORT>                Port to listen on [default: 5984]
      --host <HOST>                Host to bind to [default: 127.0.0.1]
      --db-name <NAME>             Database name [default: filename without extension]
      --admin <USER:PASSWORD>      Require admin credentials [env: ROUCHDB_ADMIN]
      --cors-origin <ORIGIN>       Allow CORS from this origin (repeatable) [env: ROUCHDB_CORS_ORIGINS]
      --allowed-host <HOST>        Also accept this name in the Host header (repeatable) [env: ROUCHDB_ALLOWED_HOSTS]
      --allow-unauthenticated      Serve on a non-loopback --host without --admin [env: ROUCHDB_ALLOW_UNAUTHENTICATED]
      --trust-proxy                Trust X-Forwarded-Proto from a reverse proxy [env: ROUCHDB_TRUST_PROXY]
      --max-request-size <BYTES>   Largest accepted request body [default: 67108864]
      --session-timeout <SECONDS>  Idle lifetime of a _session cookie [default: 600]
```

Security defaults: the server binds to `127.0.0.1`, **CORS is disabled** (so
web pages from other origins cannot use your browser to read or write the
database) and **authentication is off**. To require credentials, set
`ROUCHDB_ADMIN=user:password` (or `--admin`); clients then authenticate with
HTTP Basic auth (`http://user:password@host:5984/db`) or a `_session` cookie
(Fauxton login), and only `/`, `/_session`, `/_uuids` and `/_utils` stay
public (wrong Basic credentials are rejected even there, as in CouchDB). As in
CouchDB, a session cookie expires after 10 minutes without use
(`--session-timeout`). To let a browser app on another origin talk to the server, allow its
origin explicitly, e.g. `--cors-origin http://localhost:3000` (credentials are
allowed for listed origins; `*` allows any origin without credentials).

On a non-loopback address (`--host 0.0.0.0`) the server **refuses to start
without `--admin`**, unless you opt in explicitly with
`--allow-unauthenticated` (`ROUCHDB_ALLOW_UNAUTHENTICATED=1`). On loopback it
answers only requests whose `Host` header is `localhost`, `127.0.0.1`,
`[::1]` or the `--host` address (a 400 otherwise), so web pages cannot reach
it through DNS rebinding; declare the public name a reverse proxy forwards
with `--allowed-host`, and pass `--trust-proxy` so that session cookies are
`Secure` when the proxy says `X-Forwarded-Proto: https`. Every response
carries `X-Content-Type-Options: nosniff`. See
[Server security](https://rubylab-app.github.io/rouchdb/getting-started/installation.html#server-security)
in the book.

## CLI Tool

A command-line tool for inspecting, querying, and modifying redb database files, published as [`rouchdb-cli`](https://crates.io/crates/rouchdb-cli) (it installs the `rouchdb` binary):

```bash
cargo install rouchdb-cli
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

`dump` exports each document's winning revision with its attachments inlined as
base64, and `import` restores them. Revision history and conflicting revisions
are not exported (`dump` warns about documents with conflicts); use `replicate`
to copy a database with its full history. Read-only commands fail instead of
creating a missing `.redb` file, and every command exits with status 1 on
failure (including a `replicate` or `import` that only partly succeeded).

### Operations

```bash
rouchdb replicate mydb.redb http://admin:password@localhost:5984/mydb  # Sync
rouchdb compact mydb.redb                                              # Compact
rouchdb migrate mydb.redb                  # Upgrade a file written by rouchdb 0.4 (backup first)
```

To keep the CouchDB password out of shell history and `ps`, `replicate` reads
credentials from `ROUCHDB_USER` and `ROUCHDB_PASSWORD` for any http(s) URL that
has none of its own:

```bash
ROUCHDB_USER=admin ROUCHDB_PASSWORD=password \
  rouchdb replicate mydb.redb http://localhost:5984/mydb
```

Add `--pretty` (or `-p`) to any command for formatted JSON output.

## Crate Structure

RouchDB is a workspace of 13 crates. The first nine and `rouchdb-cli` are published on crates.io; the other three are not:

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
| `rouchdb-server` | CouchDB-compatible HTTP server with Fauxton (not published) |
| `rouchdb-cli` | Command-line tool for database inspection and CRUD |
| `rouchdb-bench` | Criterion benchmarks (not published) |
| `rouchdb-compat-tests` | Cross-version tests against rouchdb 0.4.0 from crates.io (not published) |

## Documentation

- [**Book**](https://rubylab-app.github.io/rouchdb/) — guides, reference, and architecture docs
- [**API Docs**](https://docs.rs/rouchdb) — generated Rust docs

## Development

```bash
# Run tests
cargo test --workspace

# Lint (the toolchain is pinned in rust-toolchain.toml)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# Integration tests (require CouchDB)
docker compose up -d
bash scripts/test-couchdb.sh

# Benchmarks (criterion)
cargo bench -p rouchdb-bench
```

## License

MIT
