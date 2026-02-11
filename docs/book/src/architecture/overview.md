# Architecture Overview

RouchDB is a local-first document database for Rust that implements the CouchDB
replication protocol. It is the Rust equivalent of PouchDB: applications store
data locally (in-memory, on disk, or against a remote CouchDB) and synchronize
between replicas using the same protocol that CouchDB uses natively.

## Design Philosophy

**Local-first.** Reads and writes hit local storage. The network is only
involved when you explicitly replicate.

**Adapter pattern.** Every storage backend implements a single `Adapter` trait.
Application code does not depend on a concrete backend -- you can swap
in-memory storage for disk-backed storage for a remote CouchDB endpoint without
changing a line of business logic.

**CouchDB compatibility.** Revision trees, the winning-rev algorithm, the
`_changes` feed, `_revs_diff`, `_bulk_get`, `new_edits=false` -- all of the
machinery required by the CouchDB replication protocol is present and matches
CouchDB/PouchDB semantics exactly.

**serde_json::Value as the document model.** Documents are dynamic JSON. The
crate does not impose any schema; users may layer typed Rust structs on top via
Serde when desired.

## Crate Dependency Graph

```
                      +------------------+
                      |     rouchdb      |  (umbrella: re-exports everything)
                      +------------------+
                     /  |    |    |    \   \
                    /   |    |    |     \   \
          +-----------+ +--------+ +----------+ +-----------+ +--------+
          | adapter-  | | changes| | replica- | |   query   | | views  |
          |   redb    | |        | |   tion   | |           | |        |
          +-----------+ +--------+ +----------+ +-----------+ +--------+
          |  adapter- |      |        |   \         |           |
          |  memory   |      |        |    +-----+  |           |
          +-----------+      |        |          |  |           |
          |  adapter- |      |        |          |  |           |
          |   http    |      |        |          |  |           |
          +-----------+      |        |          |  |           |
                \            |       /          /  /           /
                 \           |      /          /  /           /
                  +----------+-----+----------+--+-----------+
                  |               rouchdb-core                 |
                  +--------------------------------------------+
```

All arrows point downward. Every crate ultimately depends on `rouchdb-core`.
The `rouchdb` umbrella crate depends on all other eight crates and re-exports
their public APIs.

## The Nine Crates

### 1. `rouchdb-core`

The foundation. Contains:

- **`Adapter` trait** -- the async interface every storage backend implements
  (`info`, `get`, `bulk_docs`, `changes`, `revs_diff`, `bulk_get`,
  `put_local`/`get_local`, `compact`, `destroy`).
- **Document types** -- `Document`, `Revision`, `Seq`, and all the
  option/response structs (`GetOptions`, `BulkDocsOptions`, `ChangesOptions`,
  `AllDocsOptions`, etc.).
- **Revision tree** (`rev_tree.rs`) -- `RevTree`, `RevPath`, `RevNode`,
  traversal helpers, leaf collection, ancestry lookup.
- **Merge algorithm** (`merge.rs`) -- merge incoming revision paths, determine
  the winning rev, collect conflicts, stem old revisions.
- **Collation** (`collation.rs`) -- CouchDB-compatible comparison and
  lexicographic encoding of JSON values for ordered storage.
- **Error types** -- `RouchError` enum with `thiserror`.

External dependencies: `async-trait`, `base64`, `serde`, `serde_json`,
`thiserror`.

### 2. `rouchdb-adapter-memory`

An in-memory `Adapter` implementation. All state lives in `Arc<RwLock<...>>`
behind a Tokio read-write lock. Primarily used for testing and as a reference
implementation.

Dependencies: `rouchdb-core`, `tokio`, `md-5`, `uuid`, `base64`.

### 3. `rouchdb-adapter-redb`

Persistent local storage backed by [redb](https://github.com/cberner/redb), a
pure-Rust embedded key-value store with ACID transactions. Six tables store
document metadata, revision data, the changes log, local documents,
attachments, and global metadata.

Dependencies: `rouchdb-core`, `redb`, `tokio`, `serde`, `serde_json`, `md-5`,
`uuid`.

See [Storage Layer](storage-layer.md) for full table schema documentation.

### 4. `rouchdb-adapter-http`

A remote `Adapter` that talks to a CouchDB (or CouchDB-compatible) server over
HTTP using `reqwest`. Translates each `Adapter` method into the corresponding
CouchDB REST endpoint.

Dependencies: `rouchdb-core`, `reqwest` (with `json` and `cookies` features),
`percent-encoding`, `serde`, `serde_json`.

### 5. `rouchdb-changes`

Implements the live/continuous changes feed. Wraps an `Adapter`'s one-shot
`changes()` call in a Tokio stream that polls for new changes, emitting
`ChangeEvent` items as they arrive.

Dependencies: `rouchdb-core`, `tokio`, `tokio-util`, `serde_json`.

### 6. `rouchdb-replication`

Implements the CouchDB replication protocol: reading and writing checkpoints,
computing revision diffs, fetching missing documents, and writing them to the
target with `new_edits=false`.

Dependencies: `rouchdb-core`, `rouchdb-query`, `tokio`, `tokio-util`, `serde`,
`serde_json`, `md-5`, `uuid`.

See [Replication Protocol](replication-protocol.md) for a step-by-step
walkthrough.

### 7. `rouchdb-query`

Mango selectors (`$eq`, `$gt`, `$in`, `$regex`, etc.) and map/reduce view
support. Evaluates selectors against `serde_json::Value` documents using
CouchDB collation order.

Dependencies: `rouchdb-core`, `regex`, `serde`, `serde_json`.

### 8. `rouchdb-views`

Design documents and the persistent view engine. Stores `DesignDocument`
structs (with views, filters, validate_doc_update) and provides `ViewEngine`
for incrementally-updated Rust-native map/reduce indexes.

Dependencies: `rouchdb-core`, `serde`, `serde_json`.

### 9. `rouchdb` (umbrella)

The crate users add to their `Cargo.toml`. Re-exports types from all eight
inner crates so consumers do not need to depend on individual sub-crates.

Dependencies: all of the above, plus `serde_json`, `uuid`, `async-trait`.

## Data Flow: Writing a Document

```
Application
    |
    v
rouchdb::Database::put(doc)
    |
    v
Adapter::bulk_docs(docs, BulkDocsOptions { new_edits: true })
    |
    |-- generate revision hash (MD5 of prev_rev + deleted flag + JSON body)
    |-- load existing DocRecord from storage
    |-- build new RevPath from [new_hash, prev_hash]
    |-- merge_tree(existing_tree, new_path, rev_limit)
    |     |
    |     |-- try_merge_path: find overlap, graft new nodes
    |     |-- stem: prune revisions beyond rev_limit
    |     '-- winning_rev: determine deterministic winner
    |
    |-- write DocRecord (rev tree + seq) to DOC_TABLE
    |-- write RevDataRecord (JSON body) to REV_DATA_TABLE
    |-- write ChangeRecord to CHANGES_TABLE
    '-- increment update_seq in META_TABLE
```

## Data Flow: Replication

```
Source Adapter                         Target Adapter
      |                                      |
      |--- 1. read checkpoint (both) --------|
      |                                      |
      |--- 2. source.changes(since) -------->|
      |                                      |
      |<-- 3. target.revs_diff(revs) --------|
      |                                      |
      |--- 4. source.bulk_get(missing) ----->|
      |                                      |
      |--- 5. target.bulk_docs(new_edits=false) ->|
      |                                      |
      |--- 6. write checkpoint (both) -------|
      |                                      |
      v          (repeat per batch)          v
```

Both source and target are `dyn Adapter`. This means replication works between
any combination of backends: memory-to-disk, disk-to-CouchDB, CouchDB-to-memory,
and so on.
