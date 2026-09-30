# Installation

> **Upgrading from 0.4?** 0.5 does not open `.redb` files written by 0.4 until you upgrade them once, with `rouchdb migrate app.redb` (which writes a verified backup first; `--dry-run` only reads the file) or `Database::open_with` + `UpgradePolicy::WithBackup`, with about three times the file size of free disk space; afterwards 0.4 cannot open them, and the first `compact()` deletes old revision bodies. See [Migrating from 0.4 to 0.5](../upgrading/0.4-to-0.5.md#redb-files-upgrade-once-explicitly).

## Full Package

Add RouchDB to your project with all features:

```toml
[dependencies]
rouchdb = "0.5"
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
rouchdb = { version = "0.5", default-features = false, features = ["native-tls"] }
```

With `default-features = false` and no TLS feature, only plain `http://` URLs work, and the build has no C code at all (rustls' default crypto provider, ring, compiles a small amount of bundled C and assembly). The same features exist on `rouchdb-adapter-http`. These features are new in 0.5.0 (see the [changelog](https://github.com/rubylab-app/rouchdb/blob/main/CHANGELOG.md)); 0.4.x always uses native-tls.

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
rouchdb-core = "0.5"
rouchdb-adapter-redb = "0.5"
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

# Upgrade a file written by rouchdb 0.4 (writes a verified backup first)
rouchdb migrate mydb.redb

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
      --allowed-host <HOST>        Also accept this name in the Host header (repeatable) [env: ROUCHDB_ALLOWED_HOSTS]
      --allow-unauthenticated      Serve on a non-loopback --host without --admin [env: ROUCHDB_ALLOW_UNAUTHENTICATED]
      --trust-proxy                Trust X-Forwarded-Proto from a reverse proxy [env: ROUCHDB_TRUST_PROXY]
      --max-request-size <BYTES>   Largest accepted request body [default: 67108864]
      --session-timeout <SECONDS>  Idle lifetime of a _session cookie [default: 600]
      --upgrade                    Upgrade a file written by rouchdb 0.4 (after a verified backup)
```

`rouchdb-server --help` describes every option. Switches set through the
environment take `1`, `true`, `yes`, `on` (or `0`, `false`, `no`, `off`).

The server serves a single database. `DELETE /{db}` destroys its data and the
database answers 404 until `PUT /{db}` creates it again (a restart also brings
it back, empty).

`GET /` and `GET /{db}` report a `uuid` (32 hex digits, like CouchDB's)
derived from the served database's identity (`Adapter::id()`): it is stable
across restarts on the same file, different for every file, and renewed when
`DELETE /{db}` destroys the database. HTTP clients (`Database::http`, PouchDB)
identify a database by this uuid plus its name, so two servers that serve
same-named databases are two databases to them, and replication between them
resumes from its checkpoints.

### Server security

The defaults are meant for local development: the server listens on
`127.0.0.1`, CORS is disabled and authentication is off.

- **Authentication.** Set `ROUCHDB_ADMIN=user:password` (or `--admin`) to
  require credentials: HTTP Basic auth or a `_session` cookie (`HttpOnly`,
  `SameSite=Strict`), which expires after `--session-timeout` seconds without
  use. Only `/`, `/_session`, `/_uuids` and `/_utils` stay public. Prefer the
  environment variable, so the password does not show up in the shell history
  or `ps`.
- **Listening on the network.** On an address other than loopback
  (`--host 0.0.0.0`, `::`, a LAN address or a host name other than
  `localhost`) the server **refuses to start without `--admin`**, since anyone
  who can reach the address could read, write and delete the database:

  ```bash
  ROUCHDB_ADMIN=admin:secret rouchdb-server mydb.redb --host 0.0.0.0
  ```

  If every client that can reach the address is trusted (an isolated
  container network, for example), serve without authentication explicitly
  with `--allow-unauthenticated` (or `ROUCHDB_ALLOW_UNAUTHENTICATED=1`); the
  server then prints a warning at startup.
- **CORS.** Allow browser apps on other origins explicitly with
  `--cors-origin http://localhost:3000` (repeatable). Credentials are allowed
  for listed origins; `*` allows any origin without credentials.
- **Host header (DNS rebinding).** A web page can point its own domain name at
  `127.0.0.1` after it has loaded, and then talk to a local server with
  same-origin requests, which CORS does not stop; the browser still sends the
  page's domain in the `Host` header. On a loopback `--host` the server
  therefore only answers requests addressed to `localhost`, `127.0.0.1`,
  `[::1]` or the `--host` address (with any port, in any case), and answers
  any other `Host` with a `400 bad_request`, before authentication, CORS and
  routing (`/_utils` included). A request without `Host` (HTTP/1.0) is
  served: browsers always send it. `--allowed-host` (repeatable, or
  comma-separated in `ROUCHDB_ALLOWED_HOSTS`) adds names or IP addresses,
  without port. On any other `--host` the header is checked only when
  `--allowed-host` is given, and the loopback names and the `--host` address
  are then accepted too.
- **Response headers.** Every response carries
  `X-Content-Type-Options: nosniff`, and attachments are served with
  `Content-Security-Policy: sandbox`, so an uploaded HTML page or script
  cannot run on the server's origin.
- **Not covered.** Per-database `_security` members and admins are stored but
  not enforced: with `--admin`, the admin is the only user.

#### Behind a reverse proxy

A reverse proxy (nginx, Caddy, Traefik, ...) in front of a server on loopback
usually forwards the client's `Host`, its public name. Declare that name, or
the server answers 400:

```bash
ROUCHDB_ADMIN=admin:secret rouchdb-server mydb.redb \
  --allowed-host db.example.com --trust-proxy
```

- `--trust-proxy` (`ROUCHDB_TRUST_PROXY=1`) tells the server that
  `X-Forwarded-Proto` comes from the proxy: when it says `https`, the
  `_session` cookie gets the `Secure` attribute. Without the flag the header
  does not change the cookie (a `Secure` cookie never comes back over plain
  HTTP, which would break logging in on `http://localhost`). Only use it when
  the proxy sets that header, since a client that reaches the server directly
  could send any value.
- As in CouchDB, the `Location` header of `201 Created` responses is an
  absolute URL built from `X-Forwarded-Host` and `X-Forwarded-Proto` when the
  request has them (with or without `--trust-proxy`), else from `Host`. Only
  `Host` is checked against the allowed names, so if your clients follow
  `Location`, have the proxy set `X-Forwarded-Host` (or drop the client's).

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
