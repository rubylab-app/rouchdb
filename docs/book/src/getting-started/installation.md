# Installation

## Full Package

Add RouchDB to your project with all features:

```toml
[dependencies]
rouchdb = "0.1"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

This gives you local storage (redb), HTTP client, replication, queries, and the high-level `Database` API.

## Minimal Setup

If you only need local storage without replication or HTTP:

```toml
[dependencies]
rouchdb-core = "0.1"
rouchdb-adapter-redb = "0.1"
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
| `rouchdb` | Umbrella crate — re-exports everything above |

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
