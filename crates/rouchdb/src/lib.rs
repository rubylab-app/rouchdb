//! # RouchDB
//!
//! A local-first document database with CouchDB replication protocol support.
//!
//! RouchDB is the Rust equivalent of PouchDB — it provides a local document
//! store that can sync bidirectionally with CouchDB and compatible servers.
//!
//! ## Quick Start
//!
//! ```no_run
//! use rouchdb::Database;
//!
//! # async fn example() -> rouchdb::Result<()> {
//! // In-memory database (for testing)
//! let db = Database::memory("mydb");
//!
//! // Persistent database (redb)
//! let db = Database::open("path/to/mydb.redb", "mydb")?;
//!
//! // Put a document
//! let result = db.put("doc1", serde_json::json!({"name": "Alice"})).await?;
//!
//! // Get a document
//! let doc = db.get("doc1").await?;
//!
//! // Replicate to/from CouchDB
//! let remote = Database::http("http://localhost:5984/mydb");
//! db.replicate_to(&remote).await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

// Re-export core types
pub use rouchdb_core::adapter::Adapter;
pub use rouchdb_core::document::*;
pub use rouchdb_core::error::{Result, RouchError};
pub use rouchdb_core::merge::{is_deleted, winning_rev};

// Re-export adapters
pub use rouchdb_adapter_http::HttpAdapter;
pub use rouchdb_adapter_memory::MemoryAdapter;
pub use rouchdb_adapter_redb::RedbAdapter;

// Re-export subsystems
pub use rouchdb_changes::{ChangeSender, ChangeReceiver, LiveChangesStream};
pub use rouchdb_query::{
    find, matches_selector, query_view, FindOptions, FindResponse, ReduceFn, SortField,
    ViewQueryOptions, ViewResult,
};
pub use rouchdb_replication::{replicate, ReplicationOptions, ReplicationResult, ReplicationEvent};

/// A high-level database handle that wraps any adapter implementation.
///
/// Provides a user-friendly API similar to PouchDB's JavaScript interface.
pub struct Database {
    adapter: Arc<dyn Adapter>,
}

impl Database {
    /// Create an in-memory database (data lost when dropped).
    pub fn memory(name: &str) -> Self {
        Self {
            adapter: Arc::new(MemoryAdapter::new(name)),
        }
    }

    /// Open or create a persistent database backed by redb.
    pub fn open(path: impl AsRef<Path>, name: &str) -> Result<Self> {
        let adapter = RedbAdapter::open(path, name)?;
        Ok(Self {
            adapter: Arc::new(adapter),
        })
    }

    /// Connect to a remote CouchDB instance.
    pub fn http(url: &str) -> Self {
        Self {
            adapter: Arc::new(HttpAdapter::new(url)),
        }
    }

    /// Create a database from any adapter implementation.
    pub fn from_adapter(adapter: Arc<dyn Adapter>) -> Self {
        Self { adapter }
    }

    /// Get a reference to the underlying adapter.
    pub fn adapter(&self) -> &dyn Adapter {
        self.adapter.as_ref()
    }

    // -----------------------------------------------------------------
    // Document operations
    // -----------------------------------------------------------------

    /// Get database information.
    pub async fn info(&self) -> Result<DbInfo> {
        self.adapter.info().await
    }

    /// Retrieve a document by ID.
    pub async fn get(&self, id: &str) -> Result<Document> {
        self.adapter.get(id, GetOptions::default()).await
    }

    /// Retrieve a document with options (specific rev, conflicts, etc.).
    pub async fn get_with_opts(&self, id: &str, opts: GetOptions) -> Result<Document> {
        self.adapter.get(id, opts).await
    }

    /// Create or update a document.
    ///
    /// If the document doesn't exist, creates it.
    /// If it does exist, you must provide the current `_rev` in `opts_rev`
    /// to avoid conflicts.
    pub async fn put(
        &self,
        id: &str,
        data: serde_json::Value,
    ) -> Result<DocResult> {
        let doc = Document {
            id: id.to_string(),
            rev: None,
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        let mut results = self
            .adapter
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await?;
        Ok(results.remove(0))
    }

    /// Update an existing document (requires providing the current rev).
    pub async fn update(
        &self,
        id: &str,
        rev: &str,
        data: serde_json::Value,
    ) -> Result<DocResult> {
        let revision: Revision = rev.parse()?;
        let doc = Document {
            id: id.to_string(),
            rev: Some(revision),
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        let mut results = self
            .adapter
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await?;
        Ok(results.remove(0))
    }

    /// Delete a document (requires the current rev).
    pub async fn remove(&self, id: &str, rev: &str) -> Result<DocResult> {
        let revision: Revision = rev.parse()?;
        let doc = Document {
            id: id.to_string(),
            rev: Some(revision),
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        let mut results = self
            .adapter
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await?;
        Ok(results.remove(0))
    }

    /// Write multiple documents at once.
    pub async fn bulk_docs(
        &self,
        docs: Vec<Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>> {
        self.adapter.bulk_docs(docs, opts).await
    }

    /// Query all documents.
    pub async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
        self.adapter.all_docs(opts).await
    }

    /// Get changes since a sequence number.
    pub async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
        self.adapter.changes(opts).await
    }

    // -----------------------------------------------------------------
    // Query operations
    // -----------------------------------------------------------------

    /// Run a Mango find query.
    pub async fn find(&self, opts: FindOptions) -> Result<FindResponse> {
        find(self.adapter.as_ref(), opts).await
    }

    // -----------------------------------------------------------------
    // Replication
    // -----------------------------------------------------------------

    /// Replicate from this database to the target.
    pub async fn replicate_to(&self, target: &Database) -> Result<ReplicationResult> {
        replicate(
            self.adapter.as_ref(),
            target.adapter.as_ref(),
            ReplicationOptions::default(),
        )
        .await
    }

    /// Replicate from the source to this database.
    pub async fn replicate_from(&self, source: &Database) -> Result<ReplicationResult> {
        replicate(
            source.adapter.as_ref(),
            self.adapter.as_ref(),
            ReplicationOptions::default(),
        )
        .await
    }

    /// Replicate with custom options.
    pub async fn replicate_to_with_opts(
        &self,
        target: &Database,
        opts: ReplicationOptions,
    ) -> Result<ReplicationResult> {
        replicate(self.adapter.as_ref(), target.adapter.as_ref(), opts).await
    }

    /// Bidirectional sync (replicate in both directions).
    pub async fn sync(&self, other: &Database) -> Result<(ReplicationResult, ReplicationResult)> {
        let push = self.replicate_to(other).await?;
        let pull = self.replicate_from(other).await?;
        Ok((push, pull))
    }

    // -----------------------------------------------------------------
    // Other operations
    // -----------------------------------------------------------------

    /// Compact the database.
    pub async fn compact(&self) -> Result<()> {
        self.adapter.compact().await
    }

    /// Destroy the database and all its data.
    pub async fn destroy(&self) -> Result<()> {
        self.adapter.destroy().await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn database_put_and_get() {
        let db = Database::memory("test");

        let result = db.put("doc1", serde_json::json!({"name": "Alice"})).await.unwrap();
        assert!(result.ok);
        assert_eq!(result.id, "doc1");

        let doc = db.get("doc1").await.unwrap();
        assert_eq!(doc.data["name"], "Alice");
    }

    #[tokio::test]
    async fn database_update() {
        let db = Database::memory("test");

        let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let rev = r1.rev.unwrap();

        let r2 = db.update("doc1", &rev, serde_json::json!({"v": 2})).await.unwrap();
        assert!(r2.ok);

        let doc = db.get("doc1").await.unwrap();
        assert_eq!(doc.data["v"], 2);
    }

    #[tokio::test]
    async fn database_remove() {
        let db = Database::memory("test");

        let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let rev = r1.rev.unwrap();

        let r2 = db.remove("doc1", &rev).await.unwrap();
        assert!(r2.ok);

        let err = db.get("doc1").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn database_find() {
        let db = Database::memory("test");
        db.put("alice", serde_json::json!({"name": "Alice", "age": 30})).await.unwrap();
        db.put("bob", serde_json::json!({"name": "Bob", "age": 25})).await.unwrap();

        let result = db
            .find(FindOptions {
                selector: serde_json::json!({"age": {"$gte": 28}}),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(result.docs.len(), 1);
        assert_eq!(result.docs[0]["name"], "Alice");
    }

    #[tokio::test]
    async fn database_sync() {
        let local = Database::memory("local");
        let remote = Database::memory("remote");

        local.put("doc1", serde_json::json!({"from": "local"})).await.unwrap();
        remote.put("doc2", serde_json::json!({"from": "remote"})).await.unwrap();

        let (push, pull) = local.sync(&remote).await.unwrap();
        assert!(push.ok);
        assert!(pull.ok);

        // Both should have both docs
        let local_info = local.info().await.unwrap();
        let remote_info = remote.info().await.unwrap();
        assert_eq!(local_info.doc_count, 2);
        assert_eq!(remote_info.doc_count, 2);
    }

    #[tokio::test]
    async fn database_info() {
        let db = Database::memory("test");
        db.put("a", serde_json::json!({})).await.unwrap();
        db.put("b", serde_json::json!({})).await.unwrap();

        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 2);
        assert_eq!(info.db_name, "test");
    }
}
