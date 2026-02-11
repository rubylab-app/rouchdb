# Changes Feed

The changes feed is a core CouchDB concept: it provides an ordered log of every document modification in the database. Each change has a **sequence number** (`Seq`) that increases monotonically. You can ask for all changes since a given sequence, making the changes feed the foundation for replication, live queries, and reactive UIs.

## One-Shot Changes

The simplest usage fetches all changes that have occurred since a given sequence, then returns.

```rust
use rouchdb::{Database, ChangesOptions, Seq};
use serde_json::json;

let db = Database::memory("mydb");
db.put("a", json!({"v": 1})).await?;
db.put("b", json!({"v": 2})).await?;
db.put("c", json!({"v": 3})).await?;

let response = db.changes(ChangesOptions {
    since: Seq::default(), // start from the beginning
    include_docs: true,
    ..Default::default()
}).await?;

for event in &response.results {
    println!("seq={} id={} deleted={}", event.seq, event.id, event.deleted);
    if let Some(ref doc) = event.doc {
        println!("  doc: {}", doc);
    }
    for change_rev in &event.changes {
        println!("  rev: {}", change_rev.rev);
    }
}

println!("Last seq: {}", response.last_seq);
```

### ChangesOptions

```rust
use rouchdb::{ChangesOptions, Seq};

let opts = ChangesOptions {
    since: Seq::Num(5),                          // start after sequence 5
    limit: Some(100),                             // return at most 100 changes
    descending: false,                            // chronological order
    include_docs: true,                           // embed full document bodies
    live: false,                                  // one-shot mode
    doc_ids: Some(vec!["user:alice".into()]),     // only these document IDs
    selector: None,                               // Mango selector filter
};
```

| Field | Type | Description |
|-------|------|-------------|
| `since` | `Seq` | Return changes after this sequence. `Seq::Num(0)` or `Seq::default()` means from the beginning. |
| `limit` | `Option<u64>` | Maximum number of change events to return. |
| `descending` | `bool` | Reverse the order (newest first). |
| `include_docs` | `bool` | Include the full document body in each event. |
| `live` | `bool` | Used internally by the adapter; for live streaming, use `LiveChangesStream`. |
| `doc_ids` | `Option<Vec<String>>` | Filter changes to only these document IDs. |
| `selector` | `Option<serde_json::Value>` | Mango selector — only changes matching this selector are returned. |

### ChangesResponse and ChangeEvent

The response has two fields:

- `results` -- a `Vec<ChangeEvent>`, each containing:
  - `seq` -- the sequence identifier for this change.
  - `id` -- the document ID.
  - `changes` -- a list of `ChangeRev` structs (each has a `rev` string).
  - `deleted` -- `true` if this change was a deletion.
  - `doc` -- the document body (when `include_docs` is true).
- `last_seq` -- the sequence of the last event in the batch. Pass this as `since` in subsequent calls.

### Incremental Polling

Fetch changes in pages by saving `last_seq`:

```rust
let mut since = Seq::default();

loop {
    let response = db.changes(ChangesOptions {
        since: since.clone(),
        limit: Some(50),
        ..Default::default()
    }).await?;

    if response.results.is_empty() {
        break; // caught up
    }

    for event in &response.results {
        process_change(event);
    }

    since = response.last_seq;
}
```

## The Seq Type

Sequence identifiers differ between adapters:

- **Local adapters** (memory, redb) use numeric sequences: `Seq::Num(1)`, `Seq::Num(2)`, etc.
- **CouchDB 3.x** uses opaque string sequences: `Seq::Str("13-g1AAAA...")`.

The `Seq` enum handles both transparently:

```rust
use rouchdb::Seq;

let local_seq = Seq::Num(42);
let couch_seq = Seq::Str("13-g1AAAABXeJzLYWBg...".into());

// Extract the numeric value (parses the prefix for CouchDB strings)
let n: u64 = couch_seq.as_num(); // 13

// Format for HTTP query parameters
let qs: String = couch_seq.to_query_string();

// The default is Seq::Num(0) (the beginning)
let start = Seq::default();
```

Always pass `last_seq` back as-is to `since` rather than trying to parse or increment it. This ensures correct behavior with CouchDB's opaque sequences.

## Live Changes Stream

For real-time reactivity, `LiveChangesStream` yields change events continuously, blocking when there are no new changes.

```rust
use std::sync::Arc;
use rouchdb_changes::{LiveChangesStream, ChangesStreamOptions, ChangeSender};
use rouchdb::Seq;

let db = Arc::new(rouchdb_adapter_memory::MemoryAdapter::new("mydb"));

// Create a broadcast channel for instant notifications
let (sender, receiver) = ChangeSender::new(64);

let mut stream = LiveChangesStream::new(
    db.clone(),
    Some(receiver),
    ChangesStreamOptions {
        since: Seq::default(),
        live: true,
        include_docs: true,
        limit: None,          // no limit; run forever
        ..Default::default()
    },
);

// In a loop, await the next change
loop {
    match stream.next_change().await {
        Some(event) => {
            println!("Change: {} seq={}", event.id, event.seq);
            if event.deleted {
                println!("  (deleted)");
            }
        }
        None => {
            println!("Stream ended");
            break;
        }
    }
}
```

### ChangesStreamOptions

`ChangesStreamOptions` extends the one-shot options for live mode:

```rust
use std::time::Duration;

let opts = ChangesStreamOptions {
    since: Seq::default(),
    live: true,                                // keep listening
    include_docs: false,
    doc_ids: None,
    limit: Some(1000),                         // stop after 1000 total events
    poll_interval: Duration::from_millis(500), // fallback polling interval
};
```

The `poll_interval` is used only when no broadcast channel is provided. When a `ChangeReceiver` is available, the stream blocks on the broadcast channel for instant notification instead of polling.

### How It Works

The `LiveChangesStream` operates through a simple state machine:

1. **FetchingInitial** -- on first call, fetches all changes since the given sequence.
2. **Yielding** -- returns buffered change events one at a time.
3. **Waiting** -- when the buffer is exhausted, waits for a notification (via the broadcast channel) or polls on a timer.
4. **Done** -- when the limit is reached or the channel closes.

## ChangeSender / ChangeReceiver

The `ChangeSender` and `ChangeReceiver` pair provides a Tokio broadcast channel for notifying live streams when documents change.

```rust
use rouchdb_changes::ChangeSender;
use rouchdb::Seq;

// Create the channel pair (capacity = max buffered notifications)
let (sender, initial_receiver) = ChangeSender::new(64);

// Create additional subscribers
let another_receiver = sender.subscribe();

// Notify all subscribers that a document changed
sender.notify(Seq::Num(42), "user:alice".into());
```

When integrating with a custom adapter, call `sender.notify()` after every successful write so that all `LiveChangesStream` instances wake up immediately instead of waiting for the poll interval.

## Custom Filter Closures

For flexible client-side filtering, pass a `ChangesFilter` closure that receives each `ChangeEvent` and returns `true` to include or `false` to skip:

```rust
use rouchdb::{Database, ChangesStreamOptions, ChangesFilter};
use std::sync::Arc;
use std::time::Duration;

let db = Database::memory("mydb");

let filter: ChangesFilter = Arc::new(|event| {
    event.id.starts_with("user:")
});

let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
    filter: Some(filter),
    poll_interval: Duration::from_millis(200),
    ..Default::default()
});

while let Some(event) = rx.recv().await {
    // Only user: docs arrive here
    println!("User changed: {}", event.id);
}

handle.cancel();
```

## Database::live_changes()

The `Database` struct provides a high-level `live_changes()` method that returns an `mpsc::Receiver<ChangeEvent>` and a `ChangesHandle`. This is the recommended way to consume live changes:

```rust
use rouchdb::{Database, ChangesStreamOptions};
use std::time::Duration;

let db = Database::memory("mydb");

let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
    poll_interval: Duration::from_millis(200),
    ..Default::default()
});

// Receive events from the channel
while let Some(event) = rx.recv().await {
    println!("Change: {} seq={}", event.id, event.seq);
}

// Cancel the stream when done
handle.cancel();
```

Dropping the `ChangesHandle` also cancels the stream automatically.

## Database::live_changes_events()

For applications that need lifecycle events (active, paused, errors) in addition to document changes, use `live_changes_events()`. It returns `ChangesEvent` enum variants instead of raw `ChangeEvent` structs:

```rust
use rouchdb::{Database, ChangesStreamOptions, ChangesEvent};
use std::time::Duration;

let db = Database::memory("mydb");

let (mut rx, handle) = db.live_changes_events(ChangesStreamOptions {
    include_docs: true,
    poll_interval: Duration::from_millis(200),
    ..Default::default()
});

while let Some(event) = rx.recv().await {
    match event {
        ChangesEvent::Change(ce) => {
            println!("Doc changed: {} seq={}", ce.id, ce.seq);
        }
        ChangesEvent::Complete { last_seq } => {
            println!("Caught up at seq {}", last_seq);
        }
        ChangesEvent::Error(msg) => {
            eprintln!("Error: {}", msg);
        }
        ChangesEvent::Paused => {
            println!("Waiting for new changes...");
        }
        ChangesEvent::Active => {
            println!("Processing changes...");
        }
    }
}

handle.cancel();
```

### ChangesEvent Variants

| Variant | Description |
|---------|-------------|
| `Change(ChangeEvent)` | A document was created, updated, or deleted. |
| `Complete { last_seq: Seq }` | All current changes have been processed. |
| `Error(String)` | An error occurred while fetching changes. |
| `Paused` | Waiting for new changes (no pending changes). |
| `Active` | Resumed processing after a pause. |

## Filtering by Document IDs

Both one-shot and live changes support filtering:

```rust
let response = db.changes(ChangesOptions {
    doc_ids: Some(vec!["user:alice".into(), "user:bob".into()]),
    include_docs: true,
    ..Default::default()
}).await?;

// Only changes to user:alice and user:bob are returned
```

This is useful for building reactive views that only care about a subset of documents.

## Filtering by Mango Selector

You can filter changes using a Mango selector — only changes to documents matching the selector are returned:

```rust
use rouchdb::{Database, ChangesOptions};

let db = Database::memory("mydb");

// One-shot: only changes for documents where type == "user"
let changes = db.changes(ChangesOptions {
    selector: Some(serde_json::json!({"type": "user"})),
    include_docs: true,
    ..Default::default()
}).await?;

for event in &changes.results {
    println!("{}: {:?}", event.id, event.doc);
}
```

For live changes with selector filtering:

```rust
use rouchdb::{Database, ChangesStreamOptions};
use std::time::Duration;

let db = Database::memory("mydb");

let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
    selector: Some(serde_json::json!({"type": "user"})),
    include_docs: true,
    poll_interval: Duration::from_millis(200),
    ..Default::default()
});

while let Some(event) = rx.recv().await {
    // Only user documents arrive here
    println!("User changed: {}", event.id);
}

handle.cancel();
```

When using the HTTP adapter (CouchDB), the selector is passed natively via `filter=_selector` for server-side filtering. For local adapters (memory, redb), documents are fetched with `include_docs: true` internally and filtered in Rust.
