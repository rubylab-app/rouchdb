# Plugins

RouchDB has a plugin system that lets you hook into the document lifecycle. Plugins can modify documents before they are written, react to writes after they happen, and perform cleanup when a database is destroyed.

## The Plugin Trait

```rust
use rouchdb::{Plugin, Document, DocResult, Result};
use async_trait::async_trait;

#[async_trait]
pub trait Plugin: Send + Sync {
    /// The plugin name (used for identification).
    fn name(&self) -> &str;

    /// Called before documents are written. Can modify or reject documents.
    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        Ok(()) // default: no-op
    }

    /// Called after documents are written with the results.
    async fn after_write(&self, results: &[DocResult]) -> Result<()> {
        Ok(()) // default: no-op
    }

    /// Called when the database is destroyed.
    async fn on_destroy(&self) -> Result<()> {
        Ok(()) // default: no-op
    }
}
```

## Adding Plugins

Use `with_plugin()` to register a plugin on a database:

```rust
use rouchdb::Database;
use std::sync::Arc;

let my_plugin = Arc::new(TimestampPlugin);
let db = Database::memory("mydb").with_plugin(my_plugin);
```

Note: `with_plugin()` consumes `self` and returns a new `Database`, so use the builder pattern.

Multiple plugins can be added. They execute in registration order.

## Example: Automatic Timestamps

```rust
use rouchdb::{Plugin, Document, Result};
use async_trait::async_trait;

struct TimestampPlugin;

#[async_trait]
impl Plugin for TimestampPlugin {
    fn name(&self) -> &str { "timestamp" }

    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        for doc in docs.iter_mut() {
            if let Some(obj) = doc.data.as_object_mut() {
                obj.insert(
                    "updated_at".into(),
                    serde_json::json!(chrono::Utc::now().to_rfc3339()),
                );
            }
        }
        Ok(())
    }
}
```

## Example: Validation Plugin

Return an error from `before_write` to reject the entire batch:

```rust
use rouchdb::{Plugin, Document, Result, RouchError};
use async_trait::async_trait;

struct RequireTypeField;

#[async_trait]
impl Plugin for RequireTypeField {
    fn name(&self) -> &str { "require-type" }

    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        for doc in docs {
            if doc.data.get("type").is_none() && !doc.deleted {
                return Err(RouchError::Forbidden(
                    "All documents must have a 'type' field".into(),
                ));
            }
        }
        Ok(())
    }
}
```

## Plugin Hooks

| Hook | When | Can Modify? | Can Reject? |
|------|------|-------------|-------------|
| `before_write` | Before `bulk_docs` writes | Yes (mutable `&mut Vec<Document>`) | Yes (return `Err`) |
| `after_write` | After successful writes | No (read-only `&[DocResult]`) | Yes (return `Err`) |
| `on_destroy` | When `db.destroy()` is called | N/A | Yes (return `Err`) |

Plugins are called for all write paths: `put()`, `update()`, `remove()`, `post()`, and `bulk_docs()`.
