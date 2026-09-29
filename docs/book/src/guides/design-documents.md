# Design Documents & Views

Design documents are special documents with IDs starting with `_design/`. They define views, filters, and validation functions. In RouchDB, design documents are stored as `DesignDocument` structs and can be used with the `ViewEngine` for Rust-native map/reduce queries.

## Design Document CRUD

### Creating a Design Document

```rust
use rouchdb::{Database, DesignDocument, ViewDef};

let db = Database::memory("mydb");

let ddoc = DesignDocument::new("myapp")
    .with_view(
        "by_type",
        ViewDef::new("function(doc) { emit(doc.type, 1); }").with_reduce("_count"),
    );

let result = db.put_design(ddoc).await?;
assert!(result.ok);
```

`DesignDocument` also implements `Default`, so a struct literal only lists
what it sets: `DesignDocument { id: "_design/myapp".into(), language:
Some("javascript".into()), ..Default::default() }`.

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
ddoc.views.insert("by_name".into(), ViewDef::new("function(doc) { emit(doc.name); }"));
let result = db.put_design(ddoc).await?;
```

`put_design` writes exactly the given document, like a CouchDB `PUT`. Since
`DesignDocument` keeps every member of the design document (see below), a
`get_design` + `put_design` round trip changes only what you edited, and a
member you remove from the struct is removed from the database.

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
| `views` | `HashMap<String, ViewDef>` | JavaScript views (a string `map` and optional `reduce`), by name. |
| `other_views` | `serde_json::Map<String, Value>` | The other members of `views`, verbatim: the `lib` CommonJS library and the views of a Mango (`language: "query"`) index design document, whose `map` is an object. A name in both maps is written from `views`. |
| `filters` | `HashMap<String, Value>` | Filter functions (CouchDB accepts a string or an object). |
| `validate_doc_update` | `Option<String>` | Validation function source. |
| `shows` | `HashMap<String, Value>` | Show functions. |
| `lists` | `HashMap<String, Value>` | List functions. |
| `updates` | `HashMap<String, Value>` | Update handler functions. |
| `language` | `Option<String>` | Language for the functions (e.g., `"javascript"`, `"query"`). |
| `extra` | `serde_json::Map<String, Value>` | Every other member, verbatim: `options`, `autoupdate`, `rewrites`, `_attachments`, custom fields, ... |

`ViewDef` has `map`, `reduce` and `extra` (the other members of the view,
such as `options`). Empty `views`, `filters`, `shows`, `lists` and `updates`
objects are not written back (CouchDB treats them like absent ones).

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
