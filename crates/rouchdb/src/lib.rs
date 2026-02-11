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

use tokio::sync::RwLock;

// Re-export core types
pub use rouchdb_core::adapter::Adapter;
pub use rouchdb_core::document::*;
pub use rouchdb_core::error::{Result, RouchError};
pub use rouchdb_core::merge::{is_deleted, winning_rev};

// Re-export adapters
pub use rouchdb_adapter_http::HttpAdapter;
pub use rouchdb_adapter_http::auth::{AuthClient, Session, UserContext};
pub use rouchdb_adapter_memory::MemoryAdapter;
pub use rouchdb_adapter_redb::RedbAdapter;

// Re-export subsystems
pub use rouchdb_changes::{
    ChangeReceiver, ChangeSender, ChangesEvent, ChangesFilter, ChangesHandle, ChangesStreamOptions,
    LiveChangesStream, live_changes, live_changes_events,
};
pub use rouchdb_query::{
    BuiltIndex, CreateIndexResponse, ExplainIndex, ExplainResponse, FindOptions, FindResponse,
    IndexDefinition, IndexFields, IndexInfo, ReduceFn, SortField, StaleOption, ViewQueryOptions,
    ViewResult, build_index, find, matches_selector, query_view,
};
pub use rouchdb_views::{DesignDocument, PersistentViewIndex, ViewDef, ViewEngine};

pub use rouchdb_replication::{
    ReplicationEvent, ReplicationFilter, ReplicationHandle, ReplicationOptions, ReplicationResult,
    replicate, replicate_live, replicate_with_events,
};

/// Plugin trait for extending Database behavior.
///
/// Plugins receive lifecycle hooks during database operations.
#[async_trait::async_trait]
pub trait Plugin: Send + Sync {
    /// The plugin name.
    fn name(&self) -> &str;
    /// Called before documents are written.
    async fn before_write(&self, _docs: &mut Vec<Document>) -> Result<()> {
        Ok(())
    }
    /// Called after documents are written.
    async fn after_write(&self, _results: &[DocResult]) -> Result<()> {
        Ok(())
    }
    /// Called when the database is destroyed.
    async fn on_destroy(&self) -> Result<()> {
        Ok(())
    }
}

/// A high-level database handle that wraps any adapter implementation.
///
/// Provides a user-friendly API similar to PouchDB's JavaScript interface.
pub struct Database {
    adapter: Arc<dyn Adapter>,
    indexes: Arc<RwLock<HashMap<String, BuiltIndex>>>,
    plugins: Vec<Arc<dyn Plugin>>,
}

impl Database {
    /// Create an in-memory database (data lost when dropped).
    pub fn memory(name: &str) -> Self {
        Self {
            adapter: Arc::new(MemoryAdapter::new(name)),
            indexes: Arc::new(RwLock::new(HashMap::new())),
            plugins: Vec::new(),
        }
    }

    /// Open or create a persistent database backed by redb.
    pub fn open(path: impl AsRef<Path>, name: &str) -> Result<Self> {
        let adapter = RedbAdapter::open(path, name)?;
        Ok(Self {
            adapter: Arc::new(adapter),
            indexes: Arc::new(RwLock::new(HashMap::new())),
            plugins: Vec::new(),
        })
    }

    /// Connect to a remote CouchDB instance.
    pub fn http(url: &str) -> Self {
        Self {
            adapter: Arc::new(HttpAdapter::new(url)),
            indexes: Arc::new(RwLock::new(HashMap::new())),
            plugins: Vec::new(),
        }
    }

    /// Connect to a remote CouchDB instance using an authenticated client.
    ///
    /// The `AuthClient` should have been logged in via `auth.login()` first.
    pub fn http_with_auth(url: &str, auth: &AuthClient) -> Self {
        Self {
            adapter: Arc::new(HttpAdapter::with_auth_client(url, auth)),
            indexes: Arc::new(RwLock::new(HashMap::new())),
            plugins: Vec::new(),
        }
    }

    /// Create a database from any adapter implementation.
    pub fn from_adapter(adapter: Arc<dyn Adapter>) -> Self {
        Self {
            adapter,
            indexes: Arc::new(RwLock::new(HashMap::new())),
            plugins: Vec::new(),
        }
    }

    /// Add a plugin to this database.
    pub fn with_plugin(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
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

    /// Create a new document with an auto-generated ID.
    ///
    /// Equivalent to PouchDB's `db.post(doc)`. Generates a UUID v4 as the
    /// document ID and calls `put()`.
    pub async fn post(&self, data: serde_json::Value) -> Result<DocResult> {
        let id = uuid::Uuid::new_v4().to_string();
        self.put(&id, data).await
    }

    /// Create or update a document.
    ///
    /// If the document doesn't exist, creates it.
    /// If it does exist, you must provide the current `_rev` in `opts_rev`
    /// to avoid conflicts.
    pub async fn put(&self, id: &str, data: serde_json::Value) -> Result<DocResult> {
        if id.is_empty() {
            return Err(RouchError::MissingId);
        }
        let doc = Document {
            id: id.to_string(),
            rev: None,
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        let mut results = self.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
        Ok(results.remove(0))
    }

    /// Update an existing document (requires providing the current rev).
    pub async fn update(&self, id: &str, rev: &str, data: serde_json::Value) -> Result<DocResult> {
        if id.is_empty() {
            return Err(RouchError::MissingId);
        }
        let revision: Revision = rev.parse()?;
        let doc = Document {
            id: id.to_string(),
            rev: Some(revision),
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        let mut results = self.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
        Ok(results.remove(0))
    }

    /// Delete a document (requires the current rev).
    pub async fn remove(&self, id: &str, rev: &str) -> Result<DocResult> {
        if id.is_empty() {
            return Err(RouchError::MissingId);
        }
        let revision: Revision = rev.parse()?;
        let doc = Document {
            id: id.to_string(),
            rev: Some(revision),
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        let mut results = self.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
        Ok(results.remove(0))
    }

    /// Write multiple documents at once.
    pub async fn bulk_docs(
        &self,
        mut docs: Vec<Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>> {
        for plugin in &self.plugins {
            plugin.before_write(&mut docs).await?;
        }
        let results = self.adapter.bulk_docs(docs, opts).await?;
        for plugin in &self.plugins {
            plugin.after_write(&results).await?;
        }
        Ok(results)
    }

    /// Query all documents.
    pub async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
        self.adapter.all_docs(opts).await
    }

    /// Get changes since a sequence number.
    ///
    /// If `opts.selector` is set, changes are fetched with `include_docs: true`
    /// internally and filtered by the Mango selector. Only matching changes are
    /// returned.
    pub async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
        if let Some(ref selector) = opts.selector {
            let selector = selector.clone();
            let user_wants_docs = opts.include_docs;
            let mut fetch_opts = opts;
            fetch_opts.include_docs = true;
            fetch_opts.selector = None; // Don't pass to adapter
            let mut response = self.adapter.changes(fetch_opts).await?;
            response.results.retain(|event| {
                event
                    .doc
                    .as_ref()
                    .is_some_and(|d| matches_selector(d, &selector))
            });
            if !user_wants_docs {
                for event in &mut response.results {
                    event.doc = None;
                }
            }
            Ok(response)
        } else {
            self.adapter.changes(opts).await
        }
    }

    /// Start a live (continuous) changes feed.
    ///
    /// Returns a receiver for `ChangeEvent` and a `ChangesHandle` that can be
    /// used to cancel the stream. Dropping the handle also cancels it.
    ///
    /// If `opts.selector` is set, events are post-filtered using the Mango
    /// selector — only matching changes are forwarded through the channel.
    pub fn live_changes(
        &self,
        opts: ChangesStreamOptions,
    ) -> (tokio::sync::mpsc::Receiver<ChangeEvent>, ChangesHandle) {
        if let Some(selector) = opts.selector.clone() {
            let user_wants_docs = opts.include_docs;
            let inner_opts = ChangesStreamOptions {
                include_docs: true, // Need docs for selector evaluation
                selector: None,
                ..opts
            };
            let (inner_rx, handle) = live_changes(self.adapter.clone(), inner_opts);
            let (tx, rx) = tokio::sync::mpsc::channel(64);

            tokio::spawn(async move {
                let mut inner_rx = inner_rx;
                while let Some(mut event) = inner_rx.recv().await {
                    let matches = event
                        .doc
                        .as_ref()
                        .is_some_and(|d| matches_selector(d, &selector));
                    if !matches {
                        continue;
                    }
                    if !user_wants_docs {
                        event.doc = None;
                    }
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            });

            (rx, handle)
        } else {
            live_changes(self.adapter.clone(), opts)
        }
    }

    /// Start a live changes feed with lifecycle events.
    ///
    /// Like `live_changes()` but returns `ChangesEvent` which includes
    /// `Active`, `Paused`, `Complete`, and `Error` in addition to `Change`.
    pub fn live_changes_events(
        &self,
        opts: ChangesStreamOptions,
    ) -> (tokio::sync::mpsc::Receiver<ChangesEvent>, ChangesHandle) {
        if let Some(selector) = opts.selector.clone() {
            let user_wants_docs = opts.include_docs;
            let inner_opts = ChangesStreamOptions {
                include_docs: true,
                selector: None,
                ..opts
            };
            let (inner_rx, handle) = live_changes_events(self.adapter.clone(), inner_opts);
            let (tx, rx) = tokio::sync::mpsc::channel(64);

            tokio::spawn(async move {
                let mut inner_rx = inner_rx;
                while let Some(event) = inner_rx.recv().await {
                    let forward = match &event {
                        ChangesEvent::Change(ce) => {
                            let matches = ce
                                .doc
                                .as_ref()
                                .is_some_and(|d| matches_selector(d, &selector));
                            if !matches {
                                continue;
                            }
                            if !user_wants_docs {
                                let mut ce = ce.clone();
                                ce.doc = None;
                                ChangesEvent::Change(ce)
                            } else {
                                event
                            }
                        }
                        _ => event, // Pass through lifecycle events
                    };
                    if tx.send(forward).await.is_err() {
                        break;
                    }
                }
            });

            (rx, handle)
        } else {
            live_changes_events(self.adapter.clone(), opts)
        }
    }

    // -----------------------------------------------------------------
    // Attachment operations
    // -----------------------------------------------------------------

    /// Store an attachment on a document.
    pub async fn put_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<DocResult> {
        self.adapter
            .put_attachment(doc_id, att_id, rev, data, content_type)
            .await
    }

    /// Retrieve raw attachment data.
    pub async fn get_attachment(&self, doc_id: &str, att_id: &str) -> Result<Vec<u8>> {
        self.adapter
            .get_attachment(doc_id, att_id, GetAttachmentOptions::default())
            .await
    }

    /// Retrieve raw attachment data with options.
    pub async fn get_attachment_with_opts(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>> {
        self.adapter.get_attachment(doc_id, att_id, opts).await
    }

    /// Remove an attachment from a document.
    ///
    /// Equivalent to PouchDB's `db.removeAttachment(docId, attachmentId, rev)`.
    pub async fn remove_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
    ) -> Result<DocResult> {
        self.adapter.remove_attachment(doc_id, att_id, rev).await
    }

    // -----------------------------------------------------------------
    // Query operations
    // -----------------------------------------------------------------

    /// Run a Mango find query.
    ///
    /// If a matching index exists (created via `create_index()`), it will be
    /// used to avoid a full table scan. Otherwise falls back to scanning all
    /// documents.
    pub async fn find(&self, opts: FindOptions) -> Result<FindResponse> {
        // Check if we have a usable index
        let mut indexes = self.indexes.write().await;

        // Find the name of a usable index (if any)
        let usable_name = indexes
            .iter()
            .find(|(_, idx)| {
                if idx.def.fields.is_empty() {
                    return false;
                }
                let (first_field, _) = idx.def.fields[0].field_and_direction();
                opts.selector.get(first_field).is_some()
            })
            .map(|(name, _)| name.clone());

        if let Some(name) = usable_name {
            // Rebuild the index lazily to pick up any document changes
            let def = indexes[&name].def.clone();
            let rebuilt = build_index(self.adapter.as_ref(), &def).await?;
            indexes.insert(name.clone(), rebuilt);

            let candidate_ids = indexes[&name].find_matching(&opts.selector);
            drop(indexes);

            // Fetch only the candidate docs
            let all = self
                .adapter
                .all_docs(AllDocsOptions {
                    include_docs: true,
                    keys: Some(candidate_ids),
                    ..AllDocsOptions::new()
                })
                .await?;

            let mut matched: Vec<serde_json::Value> = Vec::new();
            for row in &all.rows {
                if let Some(ref doc_json) = row.doc
                    && matches_selector(doc_json, &opts.selector)
                {
                    matched.push(doc_json.clone());
                }
            }

            // Sort
            if let Some(ref sort_fields) = opts.sort {
                matched.sort_by(|a, b| {
                    use rouchdb_core::collation::collate;
                    use rouchdb_query::SortDirection;
                    for sf in sort_fields {
                        let (field, direction) = sf.field_and_direction();
                        let va = a.get(field).unwrap_or(&serde_json::Value::Null);
                        let vb = b.get(field).unwrap_or(&serde_json::Value::Null);
                        let cmp = collate(va, vb);
                        let cmp = if direction == SortDirection::Desc {
                            cmp.reverse()
                        } else {
                            cmp
                        };
                        if cmp != std::cmp::Ordering::Equal {
                            return cmp;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
            }

            // Skip
            if let Some(skip) = opts.skip {
                matched = matched.into_iter().skip(skip as usize).collect();
            }

            // Limit
            if let Some(limit) = opts.limit {
                matched.truncate(limit as usize);
            }

            // Field projection
            if let Some(ref fields) = opts.fields {
                matched = matched
                    .into_iter()
                    .map(|doc| {
                        let mut result = serde_json::Map::new();
                        if let serde_json::Value::Object(map) = &doc {
                            for field in fields {
                                if let Some(val) = map.get(field) {
                                    result.insert(field.clone(), val.clone());
                                }
                            }
                            if let Some(id) = map.get("_id") {
                                result
                                    .entry("_id".to_string())
                                    .or_insert_with(|| id.clone());
                            }
                        }
                        serde_json::Value::Object(result)
                    })
                    .collect();
            }

            Ok(FindResponse { docs: matched })
        } else {
            drop(indexes);
            // No usable index — full table scan
            find(self.adapter.as_ref(), opts).await
        }
    }

    // -----------------------------------------------------------------
    // Index operations
    // -----------------------------------------------------------------

    /// Create a Mango index for faster queries.
    ///
    /// Equivalent to PouchDB's `db.createIndex()`. Builds the index
    /// immediately by scanning all documents.
    pub async fn create_index(&self, def: IndexDefinition) -> Result<CreateIndexResponse> {
        let name = if def.name.is_empty() {
            // Auto-generate name from fields
            let field_names: Vec<&str> = def
                .fields
                .iter()
                .map(|sf| {
                    let (f, _) = sf.field_and_direction();
                    f
                })
                .collect();
            format!("idx-{}", field_names.join("-"))
        } else {
            def.name.clone()
        };

        let mut indexes = self.indexes.write().await;
        if indexes.contains_key(&name) {
            return Ok(CreateIndexResponse {
                result: "exists".to_string(),
                name,
            });
        }

        let index_def = IndexDefinition {
            name: name.clone(),
            fields: def.fields,
            ddoc: def.ddoc,
        };

        let built = build_index(self.adapter.as_ref(), &index_def).await?;
        indexes.insert(name.clone(), built);

        Ok(CreateIndexResponse {
            result: "created".to_string(),
            name,
        })
    }

    /// Get all indexes defined on this database.
    pub async fn get_indexes(&self) -> Vec<IndexInfo> {
        let indexes = self.indexes.read().await;
        let mut result: Vec<IndexInfo> = indexes
            .values()
            .map(|idx| IndexInfo {
                name: idx.def.name.clone(),
                ddoc: idx.def.ddoc.clone(),
                def: IndexFields {
                    fields: idx.def.fields.clone(),
                },
            })
            .collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }

    /// Explain how a query would be executed without running it.
    ///
    /// Returns which index would be used and the query plan.
    pub async fn explain(&self, opts: FindOptions) -> ExplainResponse {
        let indexes = self.indexes.read().await;
        let usable = indexes.values().find(|idx| {
            if idx.def.fields.is_empty() {
                return false;
            }
            let (first_field, _) = idx.def.fields[0].field_and_direction();
            opts.selector.get(first_field).is_some()
        });

        let dbname = self.info().await.map(|i| i.db_name).unwrap_or_default();

        if let Some(index) = usable {
            ExplainResponse {
                dbname,
                index: ExplainIndex {
                    ddoc: index.def.ddoc.clone(),
                    name: index.def.name.clone(),
                    index_type: "json".into(),
                    def: IndexFields {
                        fields: index.def.fields.clone(),
                    },
                },
                selector: opts.selector,
                fields: opts.fields,
            }
        } else {
            ExplainResponse {
                dbname,
                index: ExplainIndex {
                    ddoc: None,
                    name: "_all_docs".into(),
                    index_type: "special".into(),
                    def: IndexFields { fields: vec![] },
                },
                selector: opts.selector,
                fields: opts.fields,
            }
        }
    }

    /// Delete an index by name.
    pub async fn delete_index(&self, name: &str) -> Result<()> {
        let mut indexes = self.indexes.write().await;
        indexes
            .remove(name)
            .ok_or_else(|| RouchError::NotFound(format!("index {}", name)))?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Design document operations
    // -----------------------------------------------------------------

    /// Store a design document.
    pub async fn put_design(&self, ddoc: DesignDocument) -> Result<DocResult> {
        let json = ddoc.to_json();
        let doc = Document::from_json(json)?;
        let mut results = self.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
        Ok(results.remove(0))
    }

    /// Retrieve a design document by name.
    ///
    /// Accepts either `"myapp"` or `"_design/myapp"`.
    pub async fn get_design(&self, name: &str) -> Result<DesignDocument> {
        let id = if name.starts_with("_design/") {
            name.to_string()
        } else {
            format!("_design/{}", name)
        };
        let doc = self.adapter.get(&id, GetOptions::default()).await?;
        DesignDocument::from_json(doc.to_json())
    }

    /// Delete a design document.
    pub async fn delete_design(&self, name: &str, rev: &str) -> Result<DocResult> {
        let id = if name.starts_with("_design/") {
            name.to_string()
        } else {
            format!("_design/{}", name)
        };
        self.remove(&id, rev).await
    }

    /// Remove orphaned view indexes.
    ///
    /// Scans all design documents and removes any cached indexes
    /// that no longer have a corresponding design document view.
    pub async fn view_cleanup(&self) -> Result<()> {
        // This is a no-op in the base implementation since we don't
        // store persistent view indexes in the Database struct itself.
        // The ViewEngine handles its own cleanup.
        Ok(())
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

    /// Replicate with event streaming.
    ///
    /// Same as `replicate_to()` but emits `ReplicationEvent` through the
    /// returned receiver as replication progresses.
    pub async fn replicate_to_with_events(
        &self,
        target: &Database,
        opts: ReplicationOptions,
    ) -> Result<(
        ReplicationResult,
        tokio::sync::mpsc::Receiver<ReplicationEvent>,
    )> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let result =
            replicate_with_events(self.adapter.as_ref(), target.adapter.as_ref(), opts, tx).await?;
        Ok((result, rx))
    }

    /// Start continuous (live) replication to the target.
    ///
    /// Returns a receiver for `ReplicationEvent` and a `ReplicationHandle`
    /// that can be used to cancel the replication. Dropping the handle also
    /// cancels the replication.
    pub fn replicate_to_live(
        &self,
        target: &Database,
        opts: ReplicationOptions,
    ) -> (
        tokio::sync::mpsc::Receiver<ReplicationEvent>,
        ReplicationHandle,
    ) {
        replicate_live(self.adapter.clone(), target.adapter.clone(), opts)
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

    /// Close the database and release resources.
    pub async fn close(&self) -> Result<()> {
        self.adapter.close().await
    }

    /// Compact the database.
    pub async fn compact(&self) -> Result<()> {
        self.adapter.compact().await
    }

    /// Destroy the database and all its data.
    pub async fn destroy(&self) -> Result<()> {
        for plugin in &self.plugins {
            plugin.on_destroy().await?;
        }
        self.adapter.destroy().await
    }

    /// Permanently remove document revisions.
    ///
    /// Unlike `remove()`, purged revisions are completely erased and will not
    /// be replicated to other databases.
    pub async fn purge(&self, doc_id: &str, revs: Vec<String>) -> Result<PurgeResponse> {
        let mut req = HashMap::new();
        req.insert(doc_id.to_string(), revs);
        self.adapter.purge(req).await
    }

    /// Get the security document for this database.
    pub async fn get_security(&self) -> Result<SecurityDocument> {
        self.adapter.get_security().await
    }

    /// Set the security document for this database.
    pub async fn put_security(&self, doc: SecurityDocument) -> Result<()> {
        self.adapter.put_security(doc).await
    }
}

/// A partitioned view of a database.
///
/// Scopes queries to documents whose `_id` starts with `"{partition}:"`.
pub struct Partition<'a> {
    db: &'a Database,
    name: String,
}

impl Database {
    /// Get a partitioned view of this database.
    ///
    /// All queries on the returned `Partition` are scoped to documents
    /// whose ID starts with `"{name}:"`.
    pub fn partition(&self, name: &str) -> Partition<'_> {
        Partition {
            db: self,
            name: name.to_string(),
        }
    }
}

/// Escape regex metacharacters in a string for safe use in a regex pattern.
fn regex_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if matches!(
            c,
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '|' | '^' | '$'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

impl Partition<'_> {
    /// Query all documents in this partition.
    pub async fn all_docs(&self, mut opts: AllDocsOptions) -> Result<AllDocsResponse> {
        let prefix = format!("{}:", self.name);
        let end = format!("{}:\u{ffff}", self.name);
        if opts.start_key.is_none() {
            opts.start_key = Some(prefix);
        }
        if opts.end_key.is_none() {
            opts.end_key = Some(end);
        }
        self.db.all_docs(opts).await
    }

    /// Run a Mango find query scoped to this partition.
    pub async fn find(&self, mut opts: FindOptions) -> Result<FindResponse> {
        let escaped = regex_escape(&self.name);
        let partition_filter = serde_json::json!({"_id": {"$regex": format!("^{}:", escaped)}});
        opts.selector = serde_json::json!({"$and": [opts.selector, partition_filter]});
        self.db.find(opts).await
    }

    /// Get a document by ID within this partition.
    ///
    /// Automatically prepends the partition prefix if not present.
    pub async fn get(&self, id: &str) -> Result<Document> {
        let full_id = if id.starts_with(&format!("{}:", self.name)) {
            id.to_string()
        } else {
            format!("{}:{}", self.name, id)
        };
        self.db.get(&full_id).await
    }

    /// Put a document within this partition.
    ///
    /// Automatically prepends the partition prefix if not present.
    pub async fn put(&self, id: &str, data: serde_json::Value) -> Result<DocResult> {
        let full_id = if id.starts_with(&format!("{}:", self.name)) {
            id.to_string()
        } else {
            format!("{}:{}", self.name, id)
        };
        self.db.put(&full_id, data).await
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

        let result = db
            .put("doc1", serde_json::json!({"name": "Alice"}))
            .await
            .unwrap();
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

        let r2 = db
            .update("doc1", &rev, serde_json::json!({"v": 2}))
            .await
            .unwrap();
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
        db.put("alice", serde_json::json!({"name": "Alice", "age": 30}))
            .await
            .unwrap();
        db.put("bob", serde_json::json!({"name": "Bob", "age": 25}))
            .await
            .unwrap();

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

        local
            .put("doc1", serde_json::json!({"from": "local"}))
            .await
            .unwrap();
        remote
            .put("doc2", serde_json::json!({"from": "remote"}))
            .await
            .unwrap();

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

    #[tokio::test]
    async fn database_open_redb() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let db = Database::open(&path, "test_redb").unwrap();

        db.put("doc1", serde_json::json!({"x": 1})).await.unwrap();
        let doc = db.get("doc1").await.unwrap();
        assert_eq!(doc.data["x"], 1);
    }

    #[tokio::test]
    async fn database_from_adapter_and_accessor() {
        let adapter = Arc::new(MemoryAdapter::new("custom"));
        let db = Database::from_adapter(adapter);

        let _adapter_ref = db.adapter();
        db.put("doc1", serde_json::json!({})).await.unwrap();
        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 1);
    }

    #[tokio::test]
    async fn database_get_with_opts() {
        let db = Database::memory("test");
        let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let rev = r1.rev.unwrap();

        let doc = db
            .get_with_opts(
                "doc1",
                GetOptions {
                    rev: Some(rev),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(doc.data["v"], 1);
    }

    #[tokio::test]
    async fn database_bulk_docs() {
        let db = Database::memory("test");

        let docs = vec![
            Document {
                id: "a".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"x": 1}),
                attachments: std::collections::HashMap::new(),
            },
            Document {
                id: "b".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"x": 2}),
                attachments: std::collections::HashMap::new(),
            },
        ];
        let results = db.bulk_docs(docs, BulkDocsOptions::new()).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].ok);
        assert!(results[1].ok);
    }

    #[tokio::test]
    async fn database_all_docs() {
        let db = Database::memory("test");
        db.put("a", serde_json::json!({})).await.unwrap();
        db.put("b", serde_json::json!({})).await.unwrap();

        let result = db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert_eq!(result.rows.len(), 2);
    }

    #[tokio::test]
    async fn database_changes() {
        let db = Database::memory("test");
        db.put("a", serde_json::json!({})).await.unwrap();
        db.put("b", serde_json::json!({})).await.unwrap();

        let changes = db.changes(ChangesOptions::default()).await.unwrap();
        assert_eq!(changes.results.len(), 2);
    }

    #[tokio::test]
    async fn database_replicate_to_with_opts() {
        let local = Database::memory("local");
        let remote = Database::memory("remote");

        local
            .put("doc1", serde_json::json!({"v": 1}))
            .await
            .unwrap();

        let result = local
            .replicate_to_with_opts(
                &remote,
                ReplicationOptions {
                    batch_size: 1,
                    batches_limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(result.ok);

        let doc = remote.get("doc1").await.unwrap();
        assert_eq!(doc.data["v"], 1);
    }

    #[tokio::test]
    async fn database_post() {
        let db = Database::memory("test");

        let r1 = db.post(serde_json::json!({"name": "Alice"})).await.unwrap();
        assert!(r1.ok);
        assert!(!r1.id.is_empty());

        let r2 = db.post(serde_json::json!({"name": "Bob"})).await.unwrap();
        assert!(r2.ok);
        assert_ne!(r1.id, r2.id); // Different auto-generated IDs

        let doc = db.get(&r1.id).await.unwrap();
        assert_eq!(doc.data["name"], "Alice");

        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 2);
    }

    #[tokio::test]
    async fn database_remove_attachment() {
        let db = Database::memory("test");

        let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let rev = r1.rev.unwrap();

        // remove_attachment creates a new revision even though attachment
        // tracking in the memory adapter is simplified
        let r2 = db
            .remove_attachment("doc1", "photo.jpg", &rev)
            .await
            .unwrap();
        assert!(r2.ok);
        assert!(r2.rev.is_some());
        assert_ne!(r2.rev.as_deref().unwrap(), rev);
    }

    #[tokio::test]
    async fn database_create_and_use_index() {
        let db = Database::memory("test");

        db.put("alice", serde_json::json!({"name": "Alice", "age": 30}))
            .await
            .unwrap();
        db.put("bob", serde_json::json!({"name": "Bob", "age": 25}))
            .await
            .unwrap();
        db.put("charlie", serde_json::json!({"name": "Charlie", "age": 35}))
            .await
            .unwrap();

        // Create index on "age" field
        let result = db
            .create_index(IndexDefinition {
                name: String::new(),
                fields: vec![SortField::Simple("age".into())],
                ddoc: None,
            })
            .await
            .unwrap();
        assert_eq!(result.result, "created");
        assert_eq!(result.name, "idx-age");

        // Creating same index again returns "exists"
        let result = db
            .create_index(IndexDefinition {
                name: "idx-age".into(),
                fields: vec![SortField::Simple("age".into())],
                ddoc: None,
            })
            .await
            .unwrap();
        assert_eq!(result.result, "exists");

        // Find using the index
        let found = db
            .find(FindOptions {
                selector: serde_json::json!({"age": {"$gte": 30}}),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(found.docs.len(), 2);

        // Verify get_indexes
        let indexes = db.get_indexes().await;
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name, "idx-age");

        // Delete index
        db.delete_index("idx-age").await.unwrap();
        assert!(db.delete_index("nonexistent").await.is_err());

        let indexes = db.get_indexes().await;
        assert!(indexes.is_empty());
    }

    #[tokio::test]
    async fn database_replicate_with_events() {
        let local = Database::memory("local");
        let remote = Database::memory("remote");

        local
            .put("doc1", serde_json::json!({"v": 1}))
            .await
            .unwrap();
        local
            .put("doc2", serde_json::json!({"v": 2}))
            .await
            .unwrap();

        let (result, mut rx) = local
            .replicate_to_with_events(&remote, ReplicationOptions::default())
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_written, 2);

        // Drain events
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        // Should have Active and Complete events at minimum
        assert!(events.iter().any(|e| matches!(e, ReplicationEvent::Active)));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReplicationEvent::Complete(_)))
        );
    }

    #[tokio::test]
    async fn database_live_replication() {
        let local = Database::memory("local");
        let remote = Database::memory("remote");

        // Add a doc before starting live replication
        local
            .put("doc1", serde_json::json!({"v": 1}))
            .await
            .unwrap();

        let (mut rx, handle) = local.replicate_to_live(
            &remote,
            ReplicationOptions {
                poll_interval: std::time::Duration::from_millis(50),
                live: true,
                ..Default::default()
            },
        );

        // Wait for initial replication to complete
        let mut got_complete = false;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(2));
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(ReplicationEvent::Complete(r)) => {
                            if r.docs_written > 0 {
                                got_complete = true;
                                break;
                            }
                        }
                        Some(ReplicationEvent::Paused) => {
                            // No changes, check if doc was replicated
                            if remote.get("doc1").await.is_ok() {
                                got_complete = true;
                                break;
                            }
                        }
                        None => break,
                        _ => {}
                    }
                }
                _ = &mut timeout => break,
            }
        }

        handle.cancel();
        assert!(got_complete || remote.get("doc1").await.is_ok());
    }

    #[tokio::test]
    async fn database_changes_with_selector() {
        let db = Database::memory("test");
        db.put("alice", serde_json::json!({"type": "user", "age": 30}))
            .await
            .unwrap();
        db.put(
            "inv1",
            serde_json::json!({"type": "invoice", "amount": 100}),
        )
        .await
        .unwrap();
        db.put("bob", serde_json::json!({"type": "user", "age": 25}))
            .await
            .unwrap();

        let changes = db
            .changes(ChangesOptions {
                selector: Some(serde_json::json!({"type": "user"})),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(changes.results.len(), 2);
        assert!(
            changes
                .results
                .iter()
                .all(|c| c.id == "alice" || c.id == "bob")
        );
        // Docs should NOT be included (user didn't ask for them)
        assert!(changes.results[0].doc.is_none());
    }

    #[tokio::test]
    async fn database_changes_with_selector_and_include_docs() {
        let db = Database::memory("test");
        db.put("a", serde_json::json!({"score": 10})).await.unwrap();
        db.put("b", serde_json::json!({"score": 50})).await.unwrap();
        db.put("c", serde_json::json!({"score": 90})).await.unwrap();

        let changes = db
            .changes(ChangesOptions {
                selector: Some(serde_json::json!({"score": {"$gte": 50}})),
                include_docs: true,
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(changes.results.len(), 2);
        assert!(changes.results[0].doc.is_some());
    }

    #[tokio::test]
    async fn database_live_changes_basic() {
        let db = Database::memory("test");
        db.put("a", serde_json::json!({"v": 1})).await.unwrap();

        let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
            poll_interval: std::time::Duration::from_millis(50),
            ..Default::default()
        });

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.id, "a");

        // Add another doc — picked up by polling
        db.put("b", serde_json::json!({"v": 2})).await.unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.id, "b");

        handle.cancel();
    }

    #[tokio::test]
    async fn database_live_changes_with_selector() {
        let db = Database::memory("test");
        db.put(
            "alice",
            serde_json::json!({"type": "user", "name": "Alice"}),
        )
        .await
        .unwrap();
        db.put(
            "inv1",
            serde_json::json!({"type": "invoice", "amount": 100}),
        )
        .await
        .unwrap();
        db.put("bob", serde_json::json!({"type": "user", "name": "Bob"}))
            .await
            .unwrap();

        let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
            selector: Some(serde_json::json!({"type": "user"})),
            poll_interval: std::time::Duration::from_millis(50),
            ..Default::default()
        });

        // Should only receive user docs (alice, bob), not invoice
        let e1 = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(e1.id == "alice" || e1.id == "bob");
        assert!(e1.doc.is_none()); // user didn't request include_docs

        let e2 = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(e2.id == "alice" || e2.id == "bob");
        assert_ne!(e1.id, e2.id);

        handle.cancel();
    }

    #[tokio::test]
    async fn database_compact() {
        let db = Database::memory("test");
        db.compact().await.unwrap();
    }

    #[tokio::test]
    async fn database_destroy() {
        let db = Database::memory("test");
        db.put("doc1", serde_json::json!({})).await.unwrap();
        db.destroy().await.unwrap();

        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 0);
    }
}
