# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

RouchDB is a local-first document database in Rust — the Rust equivalent of PouchDB. It stores JSON documents locally (in-memory or on disk via redb) and replicates bidirectionally with CouchDB using the standard replication protocol.

## Build & Test Commands

```bash
# Build
cargo build
cargo build -p rouchdb-core          # single crate

# Unit tests (no external deps)
cargo test                            # all
cargo test -p rouchdb-core            # single crate
cargo test -p rouchdb-core winning_rev_simple  # single test

# Lint
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Integration tests (require CouchDB)
docker compose up -d
cargo test -p rouchdb --test '*' -- --ignored
cargo test -p rouchdb --test replication replicate_memory_to_couchdb -- --ignored  # single

# Docs (mdBook)
mdbook build docs/book
mdbook serve docs/book
```

## Architecture

### Crate Dependency Graph

```
rouchdb                 ← umbrella: Database struct + re-exports + Plugin trait
├── rouchdb-core        ← traits, types, rev tree, merge, collation, errors
├── rouchdb-adapter-memory  → core
├── rouchdb-adapter-redb    → core         (persistent storage via redb)
├── rouchdb-adapter-http    → core         (CouchDB REST client via reqwest + auth)
├── rouchdb-changes         → core         (changes feed + live streaming + events)
├── rouchdb-replication     → core, query  (CouchDB replication protocol)
├── rouchdb-query           → core         (Mango selectors + map/reduce)
└── rouchdb-views           → core         (design documents + view engine)
```

### The Adapter Trait (`rouchdb-core/src/adapter.rs`)

Central abstraction. All storage backends (memory, redb, http) implement `Adapter`. Key distinction: `bulk_docs` has two modes — `new_edits: true` (normal writes, generates revisions) vs `new_edits: false` (replication mode, accepts revisions as-is without conflict checks).

### Core Algorithms

**Revision Tree** (`core/src/rev_tree.rs`, `core/src/merge.rs`): Documents have a tree of revisions. `merge_tree()` grafts incoming revision paths. `winning_rev()` picks the deterministic winner: non-deleted beats deleted, then higher generation, then lexicographic hash.

**Collation** (`core/src/collation.rs`): CouchDB-compatible ordering: `null < bool < number < string < array < object`. Numbers use a custom encoding for correct lexicographic comparison.

**Replication** (`replication/src/protocol.rs`): Standard CouchDB protocol — read checkpoint → fetch changes → revs_diff → bulk_get missing → bulk_docs(new_edits=false) → save checkpoint. Checkpoints stored as `_local` docs.

### Data Model

All documents are `serde_json::Value` (dynamic JSON). `Document` struct holds `id`, `rev`, `deleted` flag, `data` (the JSON body), and `attachments`. Revisions are `"{generation}-{md5hash}"` strings.

## Workspace Conventions

- **Edition 2024**, resolver 3, stable Rust (no nightly features)
- Workspace-level `version` in root `Cargo.toml` — crate versions must stay in sync
- Internal dependency versions must match workspace version (e.g., `rouchdb-core = { path = "../rouchdb-core", version = "0.2.0" }`)
- All async via Tokio; tests use `#[tokio::test]`
- Integration tests are `#[ignore]` — they need CouchDB at `http://admin:password@localhost:15984` (override with `COUCHDB_URL` env var)
- `MemoryAdapter` is the go-to for fast unit testing

## Publishing

All 9 crates must be published to crates.io in dependency order: core → adapter-memory, changes, adapter-redb, adapter-http → query, views, replication → rouchdb (umbrella).
