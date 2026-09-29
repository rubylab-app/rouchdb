# Storage Layer

The `rouchdb-adapter-redb` crate provides persistent local storage backed by
[redb](https://github.com/cberner/redb), a pure-Rust embedded key-value store
with ACID transactions. This document describes the on-disk format of
rouchdb 0.5 (tables, keys and values), how files written by older versions
are upgraded, and the transactional guarantees.

## Why redb

- **Pure Rust, no C dependencies.** Eliminates build complexity and
  cross-compilation issues.
- **ACID transactions.** Crash-safe reads and writes out of the box.
- **Typed tables.** `redb::TableDefinition` encodes key and value types, and
  redb checks them every time a table is opened (the format guard below
  relies on this).
- **Single-file database.** One `.redb` file per database, easy to manage.

## Table Schema

A 0.5 file has seven tables:

```
+--------------------------+----------------+-------------+-----------------+
| Constant                 | Table name     | Key type    | Value type      |
+--------------------------+----------------+-------------+-----------------+
| DOC_TABLE                | "docs"         | &str        | &[u8]           |
| REV_DATA_TABLE           | "rev_data"     | &str        | &[u8]           |
| CHANGES_TABLE            | "changes"      | u64         | &[u8]           |
| LOCAL_TABLE              | "local_docs"   | &str        | &[u8]           |
| ATTACHMENT_TABLE         | "attachments"  | &str        | &[u8]           |
| META_TABLE               | "rouchdb_meta" | &str        | &[u8]           |
| GUARD_TABLE (the guard)  | "metadata"     | &str        | FormatGuard     |
+--------------------------+----------------+-------------+-----------------+
```

Except for the guard, values are JSON (`serde_json::to_vec` /
`from_slice`) or raw attachment bytes.

### DOC_TABLE (`"docs"`)

**Purpose:** Stores each document's revision tree and current sequence
number.

**Key:** Document ID (`&str`).

**Value:** JSON `DocRecord`: the revision tree as a flat, pre-order list of
nodes, each pointing at its parent's index. Flat storage keeps
(de)serialization depth constant however long the history is.

```rust
struct DocRecord {
    revs: Vec<FlatRevNode>,
    seq: u64,
}

struct FlatRevNode {
    pos: u64,
    hash: String,
    parent: Option<u32>, // index of the parent node; absent for a root
    missing: bool,       // omitted when false; true: the body is not stored
    deleted: bool,       // omitted when false
}
```

**Example stored value** (`1-a1b2…` compacted away, `2-f7e8…` its child):

```json
{"revs":[{"pos":1,"hash":"a1b2c3d4...","missing":true},
         {"pos":2,"hash":"f7e8d9c0...","parent":0}],
 "seq":42}
```

rouchdb ≤ 0.4 stored a nested record (`{"rev_tree": [{"pos", "tree":
{"hash", "status", "deleted", "children": [...]}}], "seq"}`), two JSON levels
per generation. Such records are still read (deep ones on a thread with a
large stack and serde_json's recursion limit disabled) and are rewritten flat
the next time the document is written. A record that cannot be decoded is an
error, never a missing document.

### REV_DATA_TABLE (`"rev_data"`)

**Purpose:** Stores the body and attachment metadata of each stored
revision.

**Key:** `"{doc_id}\0{rev}"`, e.g. `"doc1\03-abc123…"`. Revision ids are
stored in canonical form (32-digit hex ids in lower case).

Document ids may themselves contain NUL (CouchDB accepts `"a\u0000b"`), so
the key range of `a` (`"a\0"` .. `"a\x01"`) also holds the keys of `a\0b`.
Revision ids never contain NUL (they are rejected), so a key in that range
whose remainder contains a NUL belongs to a longer id and is skipped when
the bodies of `a` are listed (compaction, purge).

**Value:** JSON `RevDataRecord`:

```rust
struct RevDataRecord {
    data: serde_json::Value,                        // body without _id, _rev, ...
    deleted: bool,
    attachments: HashMap<String, AttachmentRecord>, // omitted when empty
}

struct AttachmentRecord {
    content_type: String,
    digest: String,              // "md5-<base64>", the ATTACHMENT_TABLE key
    length: u64,
    revpos: u64,                 // omitted when 0 (attachments stored by 0.4)
    encoding: Option<String>,    // omitted when absent
    encoded_length: Option<u64>, // omitted when absent
}
```

### CHANGES_TABLE (`"changes"`)

**Purpose:** The changes feed. Each document has one entry, at its most
recent sequence.

**Key:** Sequence number (`u64`), incremented by 1 on every document write.

**Value:** JSON `ChangeRecord { doc_id: String, deleted: bool }` (`deleted`:
whether the document's winning revision is a deletion).

When a document is written, the entry at its previous sequence is removed
and a new one inserted, so `changes(since: N)` is a range scan over
`(N+1..)`:

```
Seq | doc_id   | deleted
----|----------|--------
  3 | "doc1"   | false      (doc1 was written at seq 1, updated at seq 3)
  4 | "doc2"   | false
  5 | "doc3"   | true       (doc3 was deleted)
```

### LOCAL_TABLE (`"local_docs"`)

**Purpose:** Local documents: not replicated, not in `all_docs` or the
changes feed, no revision tree. Replication checkpoints live here.

**Key:** The id without its `_local/` prefix.

**Value:** The JSON body. A local document written through `bulk_docs` /
`Database::put` with a `_local/` id carries its revision (`"_rev": "0-N"`)
in the body, like the server's `PUT /{db}/_local/{id}`.

rouchdb ≤ 0.4 stored `_local/…` documents written through `bulk_docs` in
`docs` like any other document; the upgrade moves them here (see below).

### ATTACHMENT_TABLE (`"attachments"`)

**Purpose:** Attachment bytes, content-addressed.

**Key:** The digest (`"md5-<base64 of the MD5>"`), as referenced by
`AttachmentRecord::digest`.

**Value:** The raw bytes.

Identical bytes are stored once, whichever documents and revisions reference
them. Bytes stay while any stored revision references them; `compact()`
deletes the rest. (rouchdb ≤ 0.4 keyed bytes by `"{doc_id}\0{name}"`, so
re-attaching a name overwrote the bytes older revisions referenced.)

### META_TABLE (`"rouchdb_meta"`)

**Purpose:** Database metadata.

**Keys:** `"meta"` (the `MetaRecord`) and `"security"` (the security
document, once one is set).

```rust
struct MetaRecord {
    update_seq: u64,    // highest sequence number
    db_uuid: String,    // random, reset by destroy()
    schema: u32,        // on-disk layout version (currently 2)
    purge_seq: u64,     // number of purge requests applied
    doc_count: u64,     // live documents, maintained on every write
    doc_del_count: u64, // deleted documents
}
```

`info()` and `all_docs().total_rows` read the counts from this record
instead of scanning documents. A file whose `schema` is higher than this
version knows is refused unchanged.

### The format guard (`"metadata"`)

rouchdb ≤ 0.4 kept its metadata in a table named `"metadata"` and opens it
as `Table<&str, &[u8]>` in `RedbAdapter::open`. A 0.4 build must never use a
0.5 file: it cannot decode flat document records, would treat those
documents as missing and replace their history on its next write, and cannot
find digest-keyed attachments.

So in a 0.5 file `"metadata"` is a guard table whose value type,
`FormatGuard`, is zero-sized and named
`rouchdb-format-2 (this file requires rouchdb >= 0.5)`. redb refuses to open
a table with a different type, so rouchdb 0.1–0.4 fail in `open` with

```
database error: metadata is of type Table<&str, rouchdb-format-2 (this file requires rouchdb >= 0.5)>
```

before writing anything. The guard is created in the same transaction that
creates or upgrades the file, `destroy()` keeps it, and its type name must
never change (every 0.5 file stores it). A future incompatible format would
install a different guard type; 0.5 reports that type's name when it cannot
open such a file.

## Serialization Approach

Structured values are JSON. This was chosen over binary formats (bincode,
MessagePack) for:

1. **Debuggability.** JSON values can be inspected with standard tools.
2. **Compatibility.** The serialized format closely mirrors what CouchDB
   stores and returns.
3. **Flexibility.** Document bodies are already `serde_json::Value`, so no
   format conversion is needed.

Document bodies may be nested up to `MAX_NESTING_DEPTH` (1000) levels; they
are decoded with a matching recursion limit.

## Opening and Initialization

`RedbAdapter::open` (and `open_with`) opens or creates the file, then looks
at it in a read transaction:

| The file has | `open` |
|--------------|--------|
| no tables (a new file) | creates, in one write transaction, the seven tables, a fresh `MetaRecord` (`update_seq` 0, new UUID, current `schema`) and the guard |
| the guard (a 0.5 file) | writes nothing (unless a table is missing); a newer `schema` is refused |
| an unguarded `"metadata"` without `schema` (rouchdb ≤ 0.4) | returns `RouchError::UpgradeRequired` and changes nothing, unless `OpenOptions::upgrade` allows the upgrade |
| an unguarded `"metadata"` with `schema` 1 or 2 (unreleased 0.5 builds) | upgrades it automatically, after a verified backup to `<file>.rouchdb-0.5-pre.bak` (unless the policy is `InPlaceNoBackup`) |
| other tables only | refuses the file ("not a rouchdb database") |
| a `"metadata"` table of another type | refuses the file, naming that type |

## Upgrading Files Written by rouchdb ≤ 0.4

`RedbAdapter::upgrade(path, policy)`, `rouchdb migrate <path>` and
`open_with` with `UpgradePolicy::WithBackup` / `InPlaceNoBackup` run the
upgrade in three steps. The file is opened with a 32 MiB page cache (redb's
default is 1 GiB) and records and attachments are handled one at a time, so
memory stays small whatever the file size; `open_with` then reopens the
upgraded file with the default cache.

**1. Analysis (read only).** From one read transaction, the upgrade works
out everything it will write, and the report:

1. Read every entry of the old `"metadata"` table.
2. Compute the digest of every attachment stored under
   `"{doc_id}\0{name}"` (reading one entry at a time).
3. Plan the move of `_local/…` documents from `docs` to `local_docs`: the
   winning revision's body, with `"_rev": "0-N"` (`N` = its generation);
   deleted ones are dropped; their bodies and change entries are removed. A
   collision with an existing local document, or a live document whose id is
   exactly `_local/` (an empty local name 0.5 cannot address), stops the
   upgrade.
4. Plan the lower-casing of upper-case 32-digit hex revision ids in revision
   trees and body keys. A revision stored under several spellings becomes one
   node (the root-to-leaf paths are merged again); of several stored bodies,
   an identical copy is dropped, and when they differ the body of the
   spelling 0.4 ranked first is kept (leaf before inner revision, live leaf
   before deleted, then the greater id in byte order: 0.4's winner order) and
   each other body is listed in `UpgradeReport::case_duplicate_bodies_discarded`.
   Documents whose winning revision differs once ids are lower case are
   counted.
5. Count live and deleted documents, and collect the report's facts
   (attachment references without bytes, old revision bodies, attachment
   bytes only old revisions reference), as the file will be after the
   upgrade.

A record that cannot be decoded stops the upgrade here with an error naming
it; nothing is skipped. `RedbAdapter::inspect_upgrade`
(`rouchdb migrate --dry-run`) is this step alone: it never opens a write
transaction, so the file is not modified (not even redb's free-page
bookkeeping) and no disk space is used.

**2. Backup** (`WithBackup`, default path `<file>.rouchdb-0.4.bak`, or
`<file>.rouchdb-0.5-pre.bak` for a development-build file). While the file
is open (redb's file lock is held), every table is copied entry by entry
from one read transaction into `<backup>.partial` with the same table types,
committed, reopened and compared entry by entry (keys and values) with the
source, synced (the file is opened for writing, which Windows requires to
flush it), then renamed to the backup path and the directory synced. The
backup path must not exist; on any error before the rename the partial copy
is removed and the source, which is only read, is unchanged. If only the
directory sync fails, the backup is complete and the upgrade goes on with a
warning in the report. The backup opens in 0.4 exactly like the original. A
table rouchdb did not create makes the backup fail (copy the file yourself
and upgrade without a backup). After a crash, a leftover `<backup>.partial`
is not a backup (delete it); a `<backup>` is complete, as it is only renamed
into place once verified.

**3. Upgrade.** One write transaction with two-phase commit applies the
plan, so the file is upgraded completely or not at all (an error or a crash
before the commit leaves it as it was): re-key
the attachment bytes, move the `_local/` documents, rewrite the revision
trees and body keys, write the `MetaRecord` (and the security document, if
any) to `rouchdb_meta`, delete the old `"metadata"` table and create the
guard. If this step fails before the commit, the backup made for the
attempt is removed (it would block a retry); if the commit itself fails, it
is kept, since the file may then be either the old one or the upgraded one.

**Disk space.** redb copies every page the transaction changes and keeps
the old pages until the commit, so the upgrade needs about twice the file
size of free space; the backup needs about the file size again (about three
times in total).

**The first `compact()` after the upgrade** deletes every non-leaf body 0.4
kept (0.4's `compact` did nothing) and the attachment bytes only those bodies
referenced.

## Write Serialization

Every storage operation runs on Tokio's blocking thread pool
(`spawn_blocking`), since redb commits fsync. Writers first take an async
`Mutex`, so waiting writers do not each hold a blocking thread, then run one
redb write transaction:

```
1. Acquire the write lock
2. Begin a redb write transaction
3. Read the MetaRecord (update_seq, counts)
4. For each document:
   a. Read its DocRecord (a decode error aborts the batch)
   b. Plan the edit with rouchdb_core::write (conflicts, revision id,
      attachment inheritance, stemming)
   c. Store new attachment bytes under their digest
   d. Increment update_seq; remove the old CHANGES_TABLE entry
   e. Write the DocRecord, the RevDataRecord and the ChangeRecord; drop the
      bodies of stemmed revisions; adjust the document counts
5. Write the MetaRecord
6. Commit, then notify change subscribers
```

If any step fails, the transaction is not committed and nothing changes.

## Two Write Modes

### `new_edits=true` (Normal Writes)

Used for local application writes:

1. The provided `_rev` is checked against the tree as CouchDB does (a stale
   or unknown revision is a `conflict`).
2. The new revision id comes from `generate_rev_hash`: MD5 over the parent
   revision, the deleted flag, the body and the attachment set.
3. The path `[new, parent]` is merged into the tree and stemmed to the
   revision limit.

### `new_edits=false` (Replication Writes)

Used during replication:

1. No conflict check.
2. The revision id is accepted as sent (in canonical form).
3. With `_revisions`, the whole ancestry is merged
   (`build_path_from_revs`); `_revisions` is not stored.
4. Attachment stubs must refer to bytes already stored (`missing_stub`
   otherwise).

## Transactional Guarantees

- **Atomicity.** A `bulk_docs` call, an attachment write, a purge, a
  compaction and an upgrade are each one redb transaction.
- **Durability.** Once `commit()` returns, data is on disk (redb fsyncs).
- **Consistency.** The sequence and counts in the `MetaRecord` always match
  the documents; each document has exactly one change entry.
- **Isolation.** Reads (`begin_read`) see a consistent snapshot and are not
  blocked by writes.

## Compaction, Purge and Destroy

- `compact()` keeps only the bodies of leaf revisions (marking the others
  missing) and deletes attachment bytes no remaining body references.
- `purge()` removes leaf revisions (and ancestors no other leaf needs), their
  bodies and, when the tree becomes empty, the document.
- `destroy()` deletes and recreates every table except the guard, and resets
  the metadata (new UUID, `update_seq` 0, no security document). The file
  stays on disk, still a 0.5 file, and the handle keeps working as a new,
  empty database.

## Key Format Summary

```
docs:          "doc1"                 -> DocRecord JSON (flat; legacy nested readable)
rev_data:      "doc1\03-a1b2c3..."    -> RevDataRecord JSON
changes:       42                     -> ChangeRecord JSON
local_docs:    "replication-id-hash"  -> JSON body
attachments:   "md5-<base64>"         -> raw bytes
rouchdb_meta:  "meta" / "security"    -> MetaRecord JSON / security document
metadata:      "format"               -> FormatGuard (zero bytes; its type is the point)
```
