# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

RouchDB is a local-first document database in Rust — the Rust equivalent of PouchDB. It stores JSON documents locally (in-memory or on disk via redb) and replicates bidirectionally with CouchDB using the standard replication protocol. Pure Rust, no C dependencies.

## Build & Test Commands

```bash
# Build
cargo build                               # full workspace
cargo build -p rouchdb-core               # single crate

# Unit tests (no external deps)
cargo test                                # all crates
cargo test -p rouchdb-core                # single crate
cargo test -p rouchdb-core winning_rev_simple  # single test

# Lint (CI enforces both)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Integration tests (require CouchDB at localhost:15984)
docker compose up -d
cargo test -p rouchdb --test '*' -- --ignored
cargo test -p rouchdb --test replication replicate_memory_to_couchdb -- --ignored

# HTTP server
cargo run -p rouchdb-server -- mydb.redb --port 5984

# CLI
cargo run -p rouchdb-cli -- info mydb.redb
cargo run -p rouchdb-cli -- get mydb.redb user:alice

# Docs (mdBook)
mdbook build docs/book
mdbook serve docs/book
```

## Architecture

### Crate Dependency Graph

```
rouchdb                     <- umbrella: Database struct + re-exports + Plugin trait
├── rouchdb-core            <- traits, types, rev tree, merge, collation, errors
├── rouchdb-adapter-memory  -> core         (in-memory storage)
├── rouchdb-adapter-redb    -> core         (persistent storage via redb)
├── rouchdb-adapter-http    -> core         (CouchDB REST client via reqwest + cookie auth)
├── rouchdb-changes         -> core         (changes feed + live streaming + events)
├── rouchdb-replication     -> core, query  (CouchDB replication protocol)
├── rouchdb-query           -> core         (Mango selectors + map/reduce)
├── rouchdb-views           -> core         (design documents + view engine)
├── rouchdb-server          -> rouchdb, core (Axum HTTP server + Fauxton UI)
└── rouchdb-cli             -> rouchdb      (CLI tool for database inspection + CRUD)
```

### The Adapter Trait (`rouchdb-core/src/adapter.rs`)

Central abstraction — all storage backends implement `Adapter` (async-trait based, `Send + Sync`). Three implementations: `MemoryAdapter`, `RedbAdapter`, `HttpAdapter`.

Key methods: `get`, `bulk_docs`, `all_docs`, `changes`, `revs_diff`, `bulk_get`, `put_attachment`, `get_attachment`, `remove_attachment`, `get_local`, `put_local`, `remove_local`, `compact`, `destroy`, `purge`, `get_security`, `put_security`.

Critical distinction: `bulk_docs` has two modes via `BulkDocsOptions.new_edits`:
- `true` (default) — normal writes, generates revisions, checks conflicts
- `false` — replication mode, accepts revisions as-is, merges into rev tree

### The Database Struct (`rouchdb/src/lib.rs`)

High-level API wrapping `Arc<dyn Adapter>`. Factory methods: `memory()`, `open()`, `http()`, `from_adapter()`. All document operations (`put`, `update`, `remove`) route through `bulk_docs()` to ensure plugin hooks fire. Holds in-memory Mango index cache (`Arc<RwLock<HashMap<String, BuiltIndex>>>`).

### The Plugin Trait (`rouchdb/src/lib.rs`)

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;
    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> { Ok(()) }
    async fn after_write(&self, results: &[DocResult]) -> Result<()> { Ok(()) }
    async fn on_destroy(&self) -> Result<()> { Ok(()) }
}
```

### Core Algorithms

**Revision Tree** (`core/src/rev_tree.rs`, `core/src/merge.rs`): Documents have a tree of revisions. `merge_tree()` grafts incoming paths. `winning_rev()` picks the deterministic winner: non-deleted beats deleted, then higher generation, then lexicographic hash.

**Collation** (`core/src/collation.rs`): CouchDB-compatible ordering: `null < bool < number < string < array < object`. `to_indexable_string()` encodes values for correct lexicographic comparison.

**Replication** (`replication/src/protocol.rs`): Standard CouchDB protocol — generate replication ID → read checkpoint → fetch changes → revs_diff → bulk_get missing → bulk_docs(new_edits=false) → save checkpoint. Checkpoints stored as `_local` docs.

### Data Model

All documents are `serde_json::Value` (dynamic JSON). `Document` struct holds `id`, `rev`, `deleted` flag, `data` (the JSON body), and `attachments`. Revisions are `"{generation}-{md5hash}"` strings.

### Server Architecture (`rouchdb-server/src/`)

Axum 0.8 HTTP server. Single-db mode: one `.redb` file = one database. Route order matters — specific `_`-prefixed routes before `/{db}/{docid}` catch-all. State: `AppState { db: Arc<Database>, db_name: String }`. CORS configured to mirror request origin with credentials support (required for Fauxton). Error mapping: `RouchError` → CouchDB JSON `{"error": "...", "reason": "..."}`.

Reports CouchDB 3.3.3 at `GET /` so Fauxton enables all UI panels. `POST /_session` accepts both JSON and form-encoded data (Fauxton sends form data). Sets `AuthSession` cookie for browser-based auth flow.

Fauxton UI files are embedded via `rust-embed` from `crates/rouchdb-server/fauxton/` (gitignored, downloaded with `scripts/download-fauxton.sh`).

### CLI Architecture (`rouchdb-cli/src/main.rs`)

Clap-based CLI. Read commands: `info`, `get`, `all-docs`, `find`, `changes`, `dump`. Write commands: `put`, `post`, `delete`, `import`. Operations: `replicate`, `compact`. All output is JSON, `--pretty` for formatting.

## Workspace Conventions

- **Edition 2024**, resolver 3, stable Rust (no nightly features)
- Workspace-level `version` in root `Cargo.toml` — all crate versions must stay in sync
- Internal dependency versions must match workspace version (e.g., `rouchdb-core = { path = "../rouchdb-core", version = "0.2.1" }`)
- All async via Tokio; tests use `#[tokio::test]`
- Integration tests are `#[ignore]` — they need CouchDB at `http://admin:password@localhost:15984` (override with `COUCHDB_URL` env var)
- `Database::memory("test")` is the go-to for fast unit testing
- `tempfile::tempdir()` for RedbAdapter tests
- Edition 2024 requires collapsible if-let chains (no nested `if let` inside `if`)

## Error Types (`rouchdb-core/src/error.rs`)

`RouchError` enum: `NotFound(String)`, `Conflict`, `BadRequest(String)`, `Unauthorized`, `Forbidden(String)`, `InvalidRev(String)`, `MissingId`, `DatabaseExists(String)`, `DatabaseError(String)`, `Io`, `Json`. All async operations return `Result<T>` (alias for `std::result::Result<T, RouchError>`).

## Publishing

All 9 library crates must be published to crates.io in dependency order: core → adapter-memory, changes, adapter-redb, adapter-http → query, views, replication → rouchdb (umbrella). Server and CLI are `publish = false`.

## CI

GitHub Actions (`.github/workflows/ci.yml`): `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`. Docs deploy via `docs.yml` (mdBook → GitHub Pages).
