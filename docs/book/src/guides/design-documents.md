# Design Documents & Views

Design documents are special documents with IDs starting with `_design/`. They define views, filters, and validation functions. In RouchDB, design documents are stored as `DesignDocument` structs and can be used with the `ViewEngine` for Rust-native map/reduce queries.

## Design Document CRUD

### Creating a Design Document

```rust
use rouchdb::{Database, DesignDocument, ViewDef};
use std::collections::HashMap;

let db = Database::memory("mydb");

let ddoc = DesignDocument {
    id: "_design/myapp".into(),
    rev: None,
    views: {
        let mut v = HashMap::new();
        v.insert("by_type".into(), ViewDef {
            map: "function(doc) { emit(doc.type, 1); }".into(),
            reduce: Some("_count".into()),
        });
        v
    },
    filters: HashMap::new(),
    validate_doc_update: None,
    shows: HashMap::new(),
    lists: HashMap::new(),
    updates: HashMap::new(),
    language: Some("javascript".into()),
};

let result = db.put_design(ddoc).await?;
assert!(result.ok);
```

### Reading a Design Document

Pass the short name (without `_design/` prefix):

```rust
let ddoc = db.get_design("myapp").await?;
println!("ID: {}", ddoc.id); // "_design/myapp"
println!("Views: {:?}", ddoc.views.keys().collect::<Vec<_>>());
```

### Updating a Design Document

Read the document, modify it, and put it back with the current revision:

```rust
let mut ddoc = db.get_design("myapp").await?;
ddoc.views.insert("by_name".into(), ViewDef {
    map: "function(doc) { emit(doc.name); }".into(),
    reduce: None,
});
let result = db.put_design(ddoc).await?;
```

### Deleting a Design Document

```rust
let ddoc = db.get_design("myapp").await?;
let rev = ddoc.rev.unwrap();
db.delete_design("myapp", &rev).await?;
```

## DesignDocument Fields

| Field | Type | Description |
|-------|------|-------------|
| `id` | `String` | Must start with `_design/`. |
| `rev` | `Option<String>` | Current revision (set after reading). |
| `views` | `HashMap<String, ViewDef>` | Named view definitions with map and optional reduce. |
| `filters` | `HashMap<String, String>` | Named filter functions. |
| `validate_doc_update` | `Option<String>` | Validation function source. |
| `shows` | `HashMap<String, String>` | Show functions. |
| `lists` | `HashMap<String, String>` | List functions. |
| `updates` | `HashMap<String, String>` | Update handler functions. |
| `language` | `Option<String>` | Language for the functions (e.g., `"javascript"`). |

## ViewEngine (Rust-Native Views)

For local databases, RouchDB provides a `ViewEngine` that runs map/reduce using Rust closures instead of JavaScript. This is faster and type-safe.

### Registering a View

```rust
use rouchdb::{Database, ViewEngine, ViewQueryOptions, query_view, ReduceFn};

let db = Database::memory("mydb");
db.put("alice", serde_json::json!({"type": "user", "name": "Alice", "age": 30})).await?;
db.put("bob", serde_json::json!({"type": "user", "name": "Bob", "age": 25})).await?;
db.put("inv1", serde_json::json!({"type": "invoice", "amount": 100})).await?;

let mut engine = ViewEngine::new();

// Register a Rust map function for "myapp/by_type"
engine.register_map("myapp", "by_type", |doc| {
    let doc_type = doc.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
    vec![(serde_json::json!(doc_type), serde_json::json!(1))]
});
```

### Updating and Querying a View

The `ViewEngine` builds an index by scanning the changes feed. Call `update_index()` to build or refresh, then `get_index()` to access the results:

```rust
// Build/refresh the index
engine.update_index(db.adapter(), "myapp", "by_type").await?;

// Access the index entries
if let Some(index) = engine.get_index("myapp", "by_type") {
    for (doc_id, entries) in &index.entries {
        for (key, value) in entries {
            println!("{}: key={} value={}", doc_id, key, value);
        }
    }
}
```

For full map/reduce queries (with reduce, grouping, sorting), use the standalone `query_view()` function:

```rust
let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
    let doc_type = doc.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
    vec![(serde_json::json!(doc_type), serde_json::json!(1))]
};

let results = query_view(
    db.adapter(),
    &map_fn,
    Some(&ReduceFn::Count),
    ViewQueryOptions {
        reduce: true,
        group: true,
        ..ViewQueryOptions::new()
    },
).await?;

for row in &results.rows {
    println!("{}: {} documents", row.key, row.value);
}
```

### Incremental Updates

The `ViewEngine` tracks the last sequence number and only processes new/changed documents on subsequent `update_index()` calls, making it efficient for large databases.

### Cleanup

Remove unused view indexes:

```rust
db.view_cleanup().await?;
```
