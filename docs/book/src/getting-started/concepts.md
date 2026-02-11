# Core Concepts

Understanding these five concepts will make everything else in RouchDB click.

## Documents

A document is a JSON object identified by a unique `_id`. This is the fundamental unit of data in RouchDB.

```json
{
  "_id": "user:alice",
  "_rev": "1-abc123...",
  "name": "Alice",
  "email": "alice@example.com",
  "age": 30
}
```

- `_id` — unique identifier you choose (or RouchDB generates a UUID)
- `_rev` — revision string managed by RouchDB (never set this manually)
- Everything else is your data, stored as `serde_json::Value`

In Rust, you work with the `Document` struct:

```rust
pub struct Document {
    pub id: String,
    pub rev: Option<Revision>,
    pub deleted: bool,
    pub data: serde_json::Value,
    pub attachments: HashMap<String, AttachmentMeta>,
}
```

## Revisions

Every time a document is created or updated, RouchDB assigns it a new revision. Revisions look like `{generation}-{hash}`:

```
1-9a2c3b4d5e6f...    (first version)
2-7f8e9d0c1b2a...    (after first update)
3-3a4b5c6d7e8f...    (after second update)
```

- The **generation** (`1`, `2`, `3`...) counts how many times the document has been modified.
- The **hash** is an MD5 digest of the document's content, making it deterministic.

**Why revisions matter:**

1. **Conflict detection** — when you update a document, you must provide the current `_rev`. If someone else updated it first, you get a `Conflict` error.
2. **Replication** — revisions let two databases figure out which changes they're missing.
3. **History** — RouchDB keeps a tree of revisions (not the full data — old revisions are compacted away).

## Conflicts

Conflicts happen when the same document is modified on two different replicas before they sync.

```
         Replica A              Replica B
            |                      |
        doc rev 1-abc          doc rev 1-abc
            |                      |
        update to 2-def        update to 2-ghi
            |                      |
            +-------  sync  -------+
            |                      |
        conflict! two rev-2 branches
```

RouchDB handles this the same way CouchDB does:

1. **Deterministic winner** — one revision is picked as the "winner" using a consistent algorithm (non-deleted beats deleted, higher generation wins, ties broken by lexicographic hash comparison).
2. **No data loss** — the "losing" revision is kept as a conflict. You can read it and resolve it.
3. **Application-level resolution** — your code decides how to merge conflicting changes.

Read more in the [Conflict Resolution](../guides/conflict-resolution.md) guide.

## Adapters

An adapter is a storage backend. RouchDB provides three built-in adapters behind a single `Adapter` trait:

| Adapter | Constructor | Use case |
|---------|------------|----------|
| **Memory** | `Database::memory("name")` | Tests, ephemeral caches |
| **Redb** | `Database::open("path.redb", "name")` | Persistent local storage |
| **HTTP** | `Database::http("http://...")` | Remote CouchDB server |

All three implement the same trait, so your code works identically regardless of the backend:

```rust
// This function works with any adapter
async fn count_docs(db: &Database) -> rouchdb::Result<u64> {
    let info = db.info().await?;
    Ok(info.doc_count)
}
```

You can also implement the `Adapter` trait yourself for custom backends (SQLite, S3, etc.).

## Changes Feed

Every mutation (create, update, delete) gets a **sequence number**. The changes feed lets you query "what changed since sequence X?":

```rust
use rouchdb::{Database, ChangesOptions, Seq};

async fn example(db: &Database) -> rouchdb::Result<()> {
    let changes = db.changes(ChangesOptions {
        since: Seq::Num(0), // from the beginning
        include_docs: true,
        ..Default::default()
    }).await?;

    for change in &changes.results {
        println!("seq {}: {} {}",
            change.seq,
            change.id,
            if change.deleted { "(deleted)" } else { "" }
        );
    }

    Ok(())
}
```

The changes feed is also the backbone of replication — it's how two databases discover what they need to sync.

For live updates (streaming changes as they happen), see the [Changes Feed](../guides/changes-feed.md) guide.
