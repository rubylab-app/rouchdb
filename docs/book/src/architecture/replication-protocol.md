# Replication Protocol

RouchDB implements the CouchDB replication protocol, enabling bidirectional
synchronization between any two `Adapter` implementations. The protocol is
defined in `rouchdb-replication/src/protocol.rs` (the replication loop) and
`rouchdb-replication/src/checkpoint.rs` (checkpoint management).

Because both source and target are `dyn Adapter`, replication works between
any combination of backends: memory-to-memory, redb-to-CouchDB,
CouchDB-to-memory, and so on.

## Overview

The replication protocol is a batched, checkpoint-based, one-directional data
transfer. To achieve bidirectional sync, you run two replications in opposite
directions.

## Sequence Diagram

```
 Source                  Replicator                    Target
   |                        |                            |
   |                   1. Generate replication ID        |
   |                        |                            |
   |<-- get_local(rep_id) --|-- get_local(rep_id) ------>|
   |--- checkpoint_doc ---->|<--- checkpoint_doc --------|
   |                        |                            |
   |                   2. Compare checkpoints            |
   |                      (find `since` seq)             |
   |                        |                            |
   |                   === batch loop ===                |
   |                        |                            |
   |<-- changes(since,      |                            |
   |        limit=batch) ---|                            |
   |--- change_events ----->|                            |
   |                        |                            |
   |                        |-- revs_diff(revs) -------->|
   |                        |<--- missing revs ----------|
   |                        |                            |
   |<-- bulk_get(missing) --|                            |
   |--- docs + _revisions ->|                            |
   |                        |                            |
   |                        |-- bulk_docs(docs,          |
   |                        |     new_edits=false) ----->|
   |                        |<--- write results ---------|
   |                        |                            |
   |<-- put_local(cp) ------|---- put_local(cp) -------->|
   |                        |                            |
   |                   === end batch loop ===            |
   |                        |                            |
   |                   Return ReplicationResult          |
```

## The Seven Steps

### Step 1: Generate Replication ID

Before anything else, the replicator generates a deterministic replication
identifier by hashing the source and target database names with MD5:

```rust
fn generate_replication_id(source_id: &str, target_id: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(source_id.as_bytes());
    hasher.update(target_id.as_bytes());
    format!("{:x}", hasher.finalize())
}
```

This ID is used as the key for checkpoint documents on both sides. The same
source+target pair always produces the same ID, so a replication that is
stopped and restarted will find its previous checkpoint.

A random `session_id` (UUID v4) is also generated per replication run to
detect stale checkpoints.

### Step 2: Read Checkpoints

The replicator reads checkpoint documents from both source and target using
`get_local(replication_id)`. Checkpoint documents are stored as local
documents (`_local/{replication_id}`), which are not replicated themselves.

The checkpoint document structure:

```json
{
  "last_seq": 42,
  "session_id": "550e8400-e29b-41d4-a716-446655440000",
  "version": 1,
  "replicator": "rouchdb",
  "history": [
    { "last_seq": 42, "session_id": "550e8400-..." },
    { "last_seq": 30, "session_id": "previous-session-..." }
  ]
}
```

The `compare_checkpoints` function finds the last agreed-upon sequence:

1. **Same session:** If source and target have the same `session_id`, take the
   minimum of their two `last_seq` values. This handles the case where one
   side's checkpoint write succeeded but the other's failed.

2. **Different sessions:** Walk through the `history` arrays looking for a
   common `session_id`. When found, use the minimum `last_seq` from that
   shared session.

3. **No common session:** Return `Seq::zero()` (start from the beginning).

If either checkpoint read fails (e.g., first replication, or checkpoint was
deleted), the replicator starts from sequence 0.

### Step 3: Fetch Changes

The replicator calls `source.changes()` starting from the checkpoint sequence:

```rust
let changes = source.changes(ChangesOptions {
    since: current_seq,
    limit: Some(opts.batch_size),  // default: 100
    include_docs: false,
    ..Default::default()
}).await?;
```

The changes response contains a list of `ChangeEvent` items, each with a
document ID and its current leaf revision(s). The `last_seq` field marks the
high-water mark for this batch.

If the response is empty, replication is complete.

### Step 4: Compute `revs_diff`

The replicator builds a map of `doc_id -> [rev_strings]` from the changes and
sends it to the target:

```rust
let diff = target.revs_diff(rev_map).await?;
```

The target checks its own revision trees and returns only the revisions it is
missing. This avoids transferring documents the target already has. The
response also includes `possible_ancestors` -- revisions the target does have
that are ancestors of the missing ones, which helps the source minimize the
revision history it sends.

If the diff is empty (target has everything), the batch is skipped and the
replicator advances to the next batch.

### Step 5: Fetch Missing Documents with `bulk_get`

For each missing revision, the replicator constructs a `BulkGetItem` and
fetches the documents from the source:

```rust
let bulk_get_response = source.bulk_get(bulk_get_items).await?;
```

The source returns full document bodies along with `_revisions` metadata --
the chain of revision hashes from the requested revision back to the root.
This ancestry data is critical: it allows the target to reconstruct the
revision tree and merge it correctly.

A typical `bulk_get` response for one document looks like:

```json
{
  "_id": "doc1",
  "_rev": "3-ccc",
  "name": "Alice",
  "_revisions": {
    "start": 3,
    "ids": ["ccc", "bbb", "aaa"]
  }
}
```

### Step 6: Write to Target with `new_edits=false`

The fetched documents are written to the target using the replication mode
of `bulk_docs`:

```rust
let write_results = target.bulk_docs(
    docs_to_write,
    BulkDocsOptions::replication(),  // new_edits: false
).await?;
```

When `new_edits=false`:

- The target does **not** generate new revision IDs. It accepts the
  revision IDs from the source as-is.
- The target does **not** check for conflicts in the traditional sense.
  Instead, it uses the `_revisions` ancestry to reconstruct a `RevPath`
  and merges it into the existing revision tree using `merge_tree`.
- If the revision already exists (was previously replicated), the merge
  returns `MergeResult::InternalNode` and the write is a no-op.
- If the revision extends an existing branch, the merge returns
  `MergeResult::NewLeaf`.
- If the revision creates a new branch (concurrent edit on a different
  replica), the merge returns `MergeResult::NewBranch`, creating a conflict
  that users can resolve later.

### Step 7: Save Checkpoint

After each batch is successfully written, the replicator saves a checkpoint
document to **both** source and target:

```rust
checkpointer.write_checkpoint(source, target, current_seq).await;
```

The checkpoint is written to both sides so that either side can resume even
if the other is unavailable. If one write fails but the other succeeds, the
replication can still resume (the `compare_checkpoints` function handles
asymmetric checkpoints gracefully).

Checkpoint writes are fire-and-forget within a batch -- the replicator only
fails if **both** writes fail.

## Batching

The replication loop processes changes in batches of `batch_size` (default
100). This provides:

- **Progress tracking:** Checkpoints are saved after each batch, so a
  replication that is interrupted can resume from the last completed batch
  rather than starting over.
- **Memory management:** Only one batch worth of documents is held in memory
  at a time.
- **Incremental progress:** The `ReplicationResult` tracks `docs_read` and
  `docs_written` across all batches.

The loop terminates when a changes response returns fewer results than
`batch_size`, indicating all changes have been consumed.

## Configuration

```rust
pub struct ReplicationOptions {
    pub batch_size: u64,                   // default: 100
    pub batches_limit: u64,                // default: 10
    pub filter: Option<ReplicationFilter>, // default: None
    pub since: Option<Seq>,                // default: None (use checkpoint)
    pub checkpoint: bool,                  // default: true
    pub live: bool,                        // default: false
    pub retry: bool,                       // default: false
    pub poll_interval: Duration,           // default: 500ms
    pub back_off_function: Option<Box<dyn Fn(u32) -> Duration + Send + Sync>>,
}
```

- `batch_size` -- number of change events to process per iteration.
- `batches_limit` -- maximum number of batches to buffer (reserved for
  future pipelining).
- `filter` -- optional filter for selective replication. Supports
  `DocIds(Vec<String>)`, `Selector(serde_json::Value)`, or
  `Custom(Arc<dyn Fn(&ChangeEvent) -> bool + Send + Sync>)`. When `DocIds` is used,
  filtering happens at the changes feed level. `Selector` filters after
  `bulk_get`. `Custom` filters after fetching changes.
- `live` -- enable continuous replication mode.
- `retry` -- automatically retry on transient errors in live mode.
- `poll_interval` -- how often to poll for new changes in live mode.
- `back_off_function` -- custom backoff for retries; receives retry count,
  returns delay duration.

## Result

```rust
pub struct ReplicationResult {
    pub ok: bool,              // true if no errors occurred
    pub docs_read: u64,        // total change events processed
    pub docs_written: u64,     // total documents written to target
    pub errors: Vec<String>,   // individual doc errors (non-fatal)
    pub last_seq: Seq,         // final sequence reached
}
```

Individual document parse errors or write errors are collected in the
`errors` vector but do not abort the replication. The `ok` field is `true`
only when `errors` is empty.

## Error Handling

**Network errors:** If `source.changes()`, `target.revs_diff()`,
`source.bulk_get()`, or `target.bulk_docs()` returns an `Err`, the entire
replication aborts and the error propagates to the caller. The last
successfully saved checkpoint allows the next attempt to resume.

**Document-level errors:** If a document cannot be parsed from the
`bulk_get` response, or if a `bulk_docs` write reports `ok: false` for a
specific document, the error message is appended to `ReplicationResult.errors`
but replication continues with the remaining documents.

**Checkpoint errors:** If writing a checkpoint fails on one side, the
replicator continues. If both sides fail, the error is returned. On the next
replication attempt, `compare_checkpoints` falls back to the most recent
common session in the history array, or to sequence 0 if no common point
exists.

**Auth errors:** These manifest as adapter-level `Err` results (e.g., HTTP
401/403 from the `http` adapter) and abort the replication immediately.

## Incremental Replication

Because checkpoints are persisted, subsequent replications between the same
source and target are incremental. They start from the last checkpointed
sequence and only process new changes. This is the key to efficient
ongoing synchronization:

```
First replication:   0 -----> 50  (processes 50 changes)
                              ^ checkpoint saved

Second replication: 50 -----> 53  (processes only 3 new changes)
                              ^ checkpoint updated
```

## Live (Continuous) Replication

The `replicate_live()` function extends the one-shot protocol into a
continuous loop:

```
 ┌─────────────────────────────────────────────────┐
 │              Live Replication Loop               │
 │                                                  │
 │  ┌──────────────┐                                │
 │  │  One-shot    │──(changes found)──→ emit       │
 │  │  replicate() │       Complete event           │
 │  └──────┬───────┘                                │
 │         │                                        │
 │    (no changes)                                  │
 │         │                                        │
 │    emit Paused                                   │
 │         │                                        │
 │    sleep(poll_interval)                          │
 │         │                                        │
 │    ┌────▼────┐                                   │
 │    │cancelled?│──yes──→ stop                     │
 │    └────┬────┘                                   │
 │         │ no                                     │
 │         └───────→ loop back                      │
 └─────────────────────────────────────────────────┘
```

Key implementation details:

- **`CancellationToken`** from `tokio_util` controls the loop. The caller
  receives a `ReplicationHandle` that wraps the token. Calling
  `handle.cancel()` or dropping the handle stops the loop.
- **Event channel:** A `tokio::sync::mpsc::Sender<ReplicationEvent>`
  streams progress events (`Active`, `Change`, `Complete`, `Paused`,
  `Error`) to the caller.
- **Retry with backoff:** When `retry: true` and an error occurs, the loop
  sleeps for `back_off_function(retry_count)` before retrying. The retry
  counter resets after a successful cycle.
- **Poll interval:** Between successful cycles where no changes were found,
  the loop sleeps for `poll_interval` before checking again.

## Event Streaming

The `replicate_with_events()` function is a one-shot replication that emits
events through an `mpsc` channel as it progresses:

- **`Active`** -- emitted when replication starts.
- **`Change { docs_read }`** -- emitted after each batch of changes is
  written to the target.
- **`Complete(ReplicationResult)`** -- emitted when replication finishes.
- **`Error(String)`** -- emitted on per-document errors (non-fatal).

This enables UI progress tracking, logging, and monitoring without blocking
the replication process.

## Bidirectional Sync

The protocol is unidirectional by design. To achieve bidirectional sync,
run two replications:

```rust
// A -> B
replicate(&adapter_a, &adapter_b, opts.clone()).await?;

// B -> A
replicate(&adapter_b, &adapter_a, opts).await?;
```

Each direction maintains its own checkpoint (different `replication_id`
because the source/target order is reversed in the hash). Conflicts created
by concurrent edits on both sides are handled naturally by the revision tree
merge algorithm -- both branches are preserved and the deterministic
winning-rev algorithm ensures convergence.
