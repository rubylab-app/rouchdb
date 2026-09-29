# Error Handling

RouchDB uses a single error enum, `RouchError`, for all error conditions across every crate in the workspace. All fallible functions return `Result<T>`, which is an alias for `std::result::Result<T, RouchError>`.

```rust
use rouchdb::{Result, RouchError};
```

---

## The Result Type

```rust
pub type Result<T> = std::result::Result<T, RouchError>;
```

Every async method on `Database` and every `Adapter` trait method returns `Result<T>`. This makes error handling consistent throughout the entire API.

---

## RouchError Variants

```rust
#[derive(Debug, Error)]
pub enum RouchError {
    NotFound(String),
    Conflict,
    BadRequest(String),
    Unauthorized,
    Forbidden(String),
    InvalidRev(String),
    MissingId,
    DatabaseExists(String),
    DatabaseError(String),
    UpgradeRequired { path: std::path::PathBuf },
    Io(#[from] std::io::Error),
    Json(#[from] serde_json::Error),
}
```

### Variant Reference

| Variant | Display Format | When It Occurs |
|---------|---------------|----------------|
| `NotFound(String)` | `"not found: {0}"` | A document, revision, attachment, or local document does not exist. The string contains the ID or a descriptive message. |
| `Conflict` | `"conflict: document update conflict"` | An update or delete was attempted without the correct current `_rev`. The document has been modified since the revision you provided. |
| `BadRequest(String)` | `"bad request: {0}"` | The request is malformed. Examples: document body is not a JSON object, invalid query parameters, or invalid selector syntax. |
| `Unauthorized` | `"unauthorized"` | Authentication is required but not provided. Returned by the HTTP adapter when CouchDB responds with 401. |
| `Forbidden(String)` | `"forbidden: {0}"` | The authenticated user does not have permission for this operation. Returned by the HTTP adapter when CouchDB responds with 403. |
| `InvalidRev(String)` | `"invalid revision format: {0}"` | A revision string could not be parsed. Revisions must be in `{pos}-{hash}` format where `pos` is a positive integer (e.g., `"3-abc123"`). |
| `MissingId` | `"missing document id"` | A document write was attempted without a document ID. |
| `DatabaseExists(String)` | `"database already exists: {0}"` | An attempt was made to create a database that already exists. |
| `DatabaseError(String)` | `"database error: {0}"` | A general database-level error (storage corruption, adapter failure, unexpected internal state). |
| `UpgradeRequired { path }` | `"{path} was written by rouchdb 0.4 or earlier and must be upgraded …"` | `Database::open` / `RedbAdapter::open` found a redb file written by rouchdb 0.4 or earlier. The file was not modified. Upgrade it once with `rouchdb migrate <path>`, `RedbAdapter::upgrade` or `Database::open_with` (see [Migrating from 0.4 to 0.5](../upgrading/0.4-to-0.5.md#redb-files-upgrade-once-explicitly)). |
| `Io(std::io::Error)` | `"io error: {0}"` | An I/O error from the underlying storage layer (file system, network). Automatically converted from `std::io::Error` via `#[from]`. |
| `Json(serde_json::Error)` | `"json error: {0}"` | A JSON serialization or deserialization error. Automatically converted from `serde_json::Error` via `#[from]`. |

---

## Matching on Specific Errors

Use Rust's `match` expression to handle different error conditions:

```rust
use rouchdb::{Database, RouchError};

async fn handle_get(db: &Database, id: &str) {
    match db.get(id).await {
        Ok(doc) => {
            println!("Found: {}", doc.data);
        }
        Err(RouchError::NotFound(_)) => {
            println!("Document {} does not exist", id);
        }
        Err(RouchError::Unauthorized) => {
            eprintln!("Authentication required");
        }
        Err(e) => {
            eprintln!("Unexpected error: {}", e);
        }
    }
}
```

---

## Common Error Patterns

### Conflict Resolution

The most common error in document databases is the update conflict. It occurs when you try to update a document but someone else has modified it since you last read it.

```rust
use rouchdb::{Database, RouchError};

async fn safe_update(db: &Database, id: &str) -> rouchdb::Result<()> {
    loop {
        // Read the current version
        let doc = db.get(id).await?;
        let rev = doc.rev.as_ref().unwrap().to_string();

        // Modify the data
        let mut data = doc.data.clone();
        data["counter"] = json!(data["counter"].as_i64().unwrap_or(0) + 1);

        // Attempt the update
        match db.update(id, &rev, data).await {
            Ok(result) => {
                println!("Updated to rev {}", result.rev.unwrap());
                return Ok(());
            }
            Err(RouchError::Conflict) => {
                // Someone else updated the doc -- retry with the new version
                println!("Conflict detected, retrying...");
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}
```

### Create-if-not-exists

```rust
use rouchdb::{Database, RouchError};

async fn ensure_doc(db: &Database, id: &str) -> rouchdb::Result<()> {
    match db.get(id).await {
        Ok(_) => {
            // Document already exists, nothing to do
            Ok(())
        }
        Err(RouchError::NotFound(_)) => {
            // Document does not exist, create it
            db.put(id, json!({"created_at": "2026-02-07"})).await?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}
```

### Handling Bulk Write Results

`bulk_docs` does not return an error for individual document failures. Instead, check each `DocResult`:

```rust
let results = db.bulk_docs(docs, BulkDocsOptions::new()).await?;

for result in &results {
    if result.ok {
        println!("Wrote {} at rev {}", result.id, result.rev.as_deref().unwrap_or("?"));
    } else {
        eprintln!(
            "Failed to write {}: {} - {}",
            result.id,
            result.error.as_deref().unwrap_or("unknown"),
            result.reason.as_deref().unwrap_or("no reason"),
        );
    }
}
```

### Using the `?` Operator

Since `RouchError` implements `std::error::Error` (via `thiserror`), it works seamlessly with the `?` operator and with error types from other crates:

```rust
async fn process(db: &Database) -> rouchdb::Result<()> {
    let doc = db.get("config").await?;         // RouchError on failure
    let info = db.info().await?;               // RouchError on failure
    db.put("status", json!({"ok": true})).await?; // RouchError on failure
    Ok(())
}
```

### Converting from External Errors

`RouchError` has automatic `From` implementations for common external error types:

| Source Type | Converts To |
|-------------|-------------|
| `std::io::Error` | `RouchError::Io` |
| `serde_json::Error` | `RouchError::Json` |

This means I/O and JSON errors from third-party code are automatically converted when using `?`:

```rust
async fn read_and_store(db: &Database, path: &str) -> rouchdb::Result<()> {
    let content = std::fs::read_to_string(path)?;  // io::Error -> RouchError::Io
    let value: serde_json::Value = serde_json::from_str(&content)?;  // -> RouchError::Json
    db.put("imported", value).await?;
    Ok(())
}
```

---

## Replication Error Handling

Replication errors are handled differently. The `replicate` function returns a `ReplicationResult` rather than failing outright for individual document errors:

```rust
let result = local.replicate_to(&remote).await?;

if result.ok {
    println!("Replication complete: {} docs written", result.docs_written);
} else {
    eprintln!("Replication completed with errors:");
    for err in &result.errors {
        eprintln!("  - {}", err);
    }
}
```

The top-level `Result` only returns `Err` for catastrophic failures (network down, source database unreachable). Individual document write failures are collected in `result.errors`, and `result.ok` is `false` if any errors occurred.

---

## Display and Debug

`RouchError` implements both `Display` (for user-facing messages) and `Debug` (for developer diagnostics):

```rust
let err = RouchError::NotFound("doc123".into());

// Display: "not found: doc123"
println!("{}", err);

// Debug: NotFound("doc123")
println!("{:?}", err);
```

All variants produce clear, actionable error messages suitable for logging.
