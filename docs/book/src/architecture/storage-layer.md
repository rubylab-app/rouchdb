# Storage Layer

The `rouchdb-adapter-redb` crate provides persistent local storage backed by
[redb](https://github.com/cberner/redb), a pure-Rust embedded key-value store
with ACID transactions. This document describes the table schema, key/value
formats, serialization approach, and transactional guarantees.

## Why redb

- **Pure Rust, no C dependencies.** Eliminates build complexity and
  cross-compilation issues.
- **ACID transactions.** Crash-safe reads and writes out of the box.
- **Typed tables.** `redb::TableDefinition` encodes key and value types at
  compile time.
- **Single-file database.** One `.redb` file per database, easy to manage.

## Table Schema

The adapter defines six tables:

```
+-------------------+---------------+-----------------+
| Table             | Key Type      | Value Type      |
+-------------------+---------------+-----------------+
| DOC_TABLE         | &str          | &[u8]           |
| REV_DATA_TABLE    | &str          | &[u8]           |
| CHANGES_TABLE     | u64           | &[u8]           |
| LOCAL_TABLE       | &str          | &[u8]           |
| ATTACHMENT_TABLE  | &str          | &[u8]           |
| META_TABLE        | &str          | &[u8]           |
+-------------------+---------------+-----------------+
```

All value types are `&[u8]` -- the adapter serializes Rust structs to JSON
bytes using `serde_json::to_vec` and deserializes with `serde_json::from_slice`.

### DOC_TABLE (`"docs"`)

**Purpose:** Stores document metadata, including the full revision tree and the
current sequence number.

**Key:** Document ID as a string (`&str`).

**Value:** JSON-serialized `DocRecord`:

```rust
struct DocRecord {
    rev_tree: Vec<SerializedRevPath>,
    seq: u64,
}

struct SerializedRevPath {
    pos: u64,
    tree: SerializedRevNode,
}

struct SerializedRevNode {
    hash: String,
    status: String,      // "available" or "missing"
    deleted: bool,
    children: Vec<SerializedRevNode>,
}
```

The `DocRecord` contains:

- `rev_tree` -- the complete revision tree, serialized as a recursive
  JSON structure. Each node stores its hash, availability status, deleted
  flag, and children. This is a direct serialization of the in-memory
  `RevTree` / `RevPath` / `RevNode` types.
- `seq` -- the most recent change sequence number for this document.
  Used to update the changes table when the document is modified (the old
  change entry at this sequence is removed and a new one is inserted).

**Example stored value:**

```json
{
  "rev_tree": [
    {
      "pos": 1,
      "tree": {
        "hash": "a1b2c3d4e5f6...",
        "status": "missing",
        "deleted": false,
        "children": [
          {
            "hash": "f7e8d9c0b1a2...",
            "status": "available",
            "deleted": false,
            "children": []
          }
        ]
      }
    }
  ],
  "seq": 42
}
```

### REV_DATA_TABLE (`"rev_data"`)

**Purpose:** Stores the actual JSON body for each revision.

**Key:** Composite key `"{doc_id}\0{rev_str}"` -- the document ID and full
revision string (e.g., `"3-abc123"`) separated by a null byte. The null byte
ensures that keys for the same document are contiguous in the table.

Document ids may themselves contain NUL (CouchDB accepts `"a\u0000b"`), so
the key range of `a` (`"a\0"` .. `"a\x01"`) also holds the keys of `a\0b`.
Revision ids never contain NUL (they are rejected), so a key in that range
whose remainder contains a NUL belongs to a longer id and is skipped when
the bodies of `a` are listed (compaction, purge).

```rust
fn rev_data_key(doc_id: &str, rev_str: &str) -> String {
    format!("{}\0{}", doc_id, rev_str)
}
```

**Value:** JSON-serialized `RevDataRecord`:

```rust
struct RevDataRecord {
    data: serde_json::Value,
    deleted: bool,
}
```

- `data` -- the document body (everything except `_id`, `_rev`, `_deleted`,
  `_attachments`, and `_revisions`).
- `deleted` -- whether this specific revision is a deletion tombstone.

Note that the `data` field stores the user's JSON as-is. CouchDB underscore
fields (`_id`, `_rev`, etc.) are stripped before storage and re-injected on
read.

### CHANGES_TABLE (`"changes"`)

**Purpose:** Implements the changes feed. Each entry represents the most
recent change for a document.

**Key:** Sequence number (`u64`). This is a monotonically increasing integer,
incremented by 1 on every document write.

**Value:** JSON-serialized `ChangeRecord`:

```rust
struct ChangeRecord {
    doc_id: String,
    deleted: bool,
}
```

When a document is updated, the adapter:

1. Removes the old change entry at the document's previous sequence number
2. Inserts a new entry at the new sequence number

This means each document appears at most once in the changes table, at its
most recent sequence. Querying `changes(since: N)` performs a range scan over
`(N+1..)`.

**Example table state after 5 writes:**

```
Seq | doc_id   | deleted
----|----------|--------
  3 | "doc1"   | false      (doc1 was written at seq 1, updated at seq 3)
  4 | "doc2"   | false
  5 | "doc3"   | true       (doc3 was deleted)
```

Sequences 1 and 2 no longer appear because those entries were replaced when
their documents were updated.

### LOCAL_TABLE (`"local_docs"`)

**Purpose:** Stores local documents that are not replicated. The primary use
case is replication checkpoints (`_local/{replication_id}`).

**Key:** Local document ID as a string (`&str`). The `_local/` prefix used
in CouchDB's HTTP API is stripped -- the key is just the ID portion.

**Value:** Raw JSON bytes (`serde_json::Value` serialized with `to_vec`).
A local document written through `bulk_docs` / `Database::put` with a
`_local/` id carries its revision (`"_rev": "0-N"`) in the body, like the
server's `PUT /{db}/_local/{id}`.

Local documents do not have revision trees or sequence numbers. They are
simple key-value pairs that can be read, written, and deleted. They do not
appear in the changes feed or in `_all_docs` results.

### ATTACHMENT_TABLE (`"attachments"`)

**Purpose:** Stores raw attachment binary data, keyed by content digest.

**Key:** Content digest as a string (`&str`), e.g., `"md5-abc123..."`.

**Value:** Raw bytes of the attachment (`&[u8]`).

Content-addressable storage means identical attachments are stored only once
regardless of how many documents reference them.

> **Note:** Attachment support in the redb adapter is not yet fully
> implemented. The table is created on initialization but the `put_attachment`
> and `get_attachment` methods currently return errors.

### META_TABLE (`"metadata"`)

**Purpose:** Global database metadata.

**Key:** Always the string `"meta"` (single-row table).

**Value:** JSON-serialized `MetaRecord`:

```rust
struct MetaRecord {
    update_seq: u64,
    db_uuid: String,
}
```

- `update_seq` -- the current highest sequence number. Incremented on every
  document write. Used by `info()` to report the database's update sequence.
- `db_uuid` -- a random UUID generated when the database is first created.
  Reset when the database is destroyed.

## Serialization Approach

All structured data is serialized to JSON bytes using `serde_json::to_vec` and
deserialized with `serde_json::from_slice`. This was chosen over binary
formats (bincode, MessagePack) for several reasons:

1. **Debuggability.** JSON values can be inspected with standard tools.
2. **Compatibility.** The serialized format closely mirrors what CouchDB
   stores and returns.
3. **Flexibility.** Document bodies are already `serde_json::Value`, so no
   format conversion is needed.

The revision tree requires a separate set of "serialized" types
(`SerializedRevPath`, `SerializedRevNode`) because the in-memory types
(`RevPath`, `RevNode`) use an enum for `RevStatus` and a struct for
`NodeOpts`. The serialized types flatten these into simple strings and bools:

```
RevStatus::Available  ->  "available"
RevStatus::Missing    ->  "missing"
NodeOpts { deleted }  ->  bool field `deleted`
```

Conversion functions handle the mapping:

```rust
fn rev_tree_to_serialized(tree: &RevTree) -> Vec<SerializedRevPath>
fn serialized_to_rev_tree(paths: &[SerializedRevPath]) -> RevTree
```

## Write Serialization

All document writes go through `bulk_docs`, which acquires a Tokio `RwLock`
before beginning a redb write transaction:

```rust
pub struct RedbAdapter {
    db: Arc<Database>,
    name: String,
    write_lock: Arc<RwLock<()>>,
}
```

The write lock is necessary because document writes are read-modify-write
operations: they must read the current `DocRecord`, merge the new revision
into the tree, and write the updated record back. Without the lock, two
concurrent writes to the same document could read the same tree, merge
independently, and one would overwrite the other's changes.

redb provides its own transaction isolation (write transactions are
serialized at the redb level), but the Tokio lock ensures that the
Rust-level read-modify-write sequence is atomic.

The flow within a single `bulk_docs` call:

```
1. Acquire write_lock
2. Begin redb write transaction
3. Read META_TABLE to get current update_seq
4. For each document:
   a. Read existing DocRecord from DOC_TABLE (if any)
   b. Deserialize revision tree
   c. Generate new revision (or accept as-is for replication)
   d. merge_tree(existing_tree, new_path, rev_limit)
   e. Increment update_seq
   f. Remove old CHANGES_TABLE entry (if document existed before)
   g. Write updated DocRecord to DOC_TABLE
   h. Write RevDataRecord to REV_DATA_TABLE
   i. Write ChangeRecord to CHANGES_TABLE
5. Write updated MetaRecord to META_TABLE
6. Commit transaction
7. Release write_lock
```

If any step fails, the redb transaction is not committed and all changes are
rolled back. The write lock is released when the `_lock` guard is dropped.

## Two Write Modes

### `new_edits=true` (Normal Writes)

Used for local application writes. The adapter:

1. Checks for conflicts -- the provided `_rev` must match the current
   winning revision.
2. Generates a new revision hash from `MD5(prev_rev + deleted + json_body)`.
3. Builds a `RevPath` with `[new_hash, prev_hash]` and merges it.

### `new_edits=false` (Replication Writes)

Used during replication. The adapter:

1. Does **not** check for conflicts.
2. Accepts the revision ID from the source document as-is.
3. If the document includes `_revisions` metadata, builds a full-ancestry
   `RevPath` using `build_path_from_revs` and merges the entire chain.
4. Strips `_revisions` from the stored document body.

This mode allows the target to reconstruct the source's revision tree
faithfully, including branches that represent conflicts.

## Transactional Guarantees

- **Atomicity.** All documents in a single `bulk_docs` call are written in
  one redb transaction. Either all succeed or none do.
- **Durability.** Once `commit()` returns, data is persisted to disk (redb
  uses fsync).
- **Consistency.** The sequence number in `META_TABLE` always matches the
  highest key in `CHANGES_TABLE`. The revision tree in `DOC_TABLE` always
  reflects all revisions whose data exists in `REV_DATA_TABLE`.
- **Isolation.** Concurrent reads (using `begin_read`) see a consistent
  snapshot and are not blocked by writes.

## Initialization

When `RedbAdapter::open` is called:

1. `Database::create` opens or creates the `.redb` file.
2. A write transaction creates all six tables (redb creates tables on first
   open within a write transaction).
3. If `META_TABLE` has no `"meta"` entry, a fresh `MetaRecord` is inserted
   with `update_seq: 0` and a new UUID.

## Destroy

`destroy()` drains all entries from all six tables and resets the metadata
to `update_seq: 0` with a fresh UUID. The database file itself is not
deleted -- it remains on disk but is empty, and the handle keeps working as
a new, empty database.

## Revision Hash Generation

```rust
fn generate_rev_hash(
    doc_data: &serde_json::Value,
    deleted: bool,
    prev_rev: Option<&str>,
) -> String
```

The hash is computed as:

```
MD5( [prev_rev_string] + ("1" if deleted else "0") + json_serialized_body )
```

This is deterministic: the same edit on the same predecessor always produces
the same hash. The hex-encoded MD5 digest becomes the hash portion of the
revision ID (e.g., `"2-a1b2c3d4..."`).

## Key Format Summary

```
DOC_TABLE:         "doc1"                   -> DocRecord JSON
REV_DATA_TABLE:    "doc1\03-a1b2c3..."      -> RevDataRecord JSON
CHANGES_TABLE:     42                        -> ChangeRecord JSON
LOCAL_TABLE:       "replication-id-hash"     -> arbitrary JSON
ATTACHMENT_TABLE:  "md5-digest..."           -> raw bytes
META_TABLE:        "meta"                    -> MetaRecord JSON
```
