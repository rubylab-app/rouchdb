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
    BuiltIndex, CompiledSelector, CreateIndexResponse, ExplainIndex, ExplainResponse, FindOptions,
    FindResponse, IndexDefinition, IndexFields, IndexInfo, ReduceFn, SortField, StaleOption,
    ViewQueryOptions, ViewResult, build_index, find, find_in_docs, matches_selector, query_view,
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
    indexes: Arc<RwLock<HashMap<String, MangoIndex>>>,
    plugins: Vec<Arc<dyn Plugin>>,
}

/// A Mango index kept up to date from the changes feed.
struct MangoIndex {
    built: BuiltIndex,
    /// Sequence of the last change applied to the index.
    last_seq: Seq,
}

/// Name of an index whose first field the selector constrains (the one
/// with the smallest name if several are usable).
fn usable_index(
    indexes: &HashMap<String, MangoIndex>,
    selector: &serde_json::Value,
) -> Option<String> {
    indexes
        .iter()
        .filter(|(_, index)| {
            index.built.def.fields.first().is_some_and(|first| {
                first
                    .try_field_and_direction()
                    .is_ok_and(|(field, _)| selector.get(field).is_some())
            })
        })
        .map(|(name, _)| name)
        .min()
        .cloned()
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
        let results = self.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
        first_result(results)
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
        let results = self.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
        first_result(results)
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
        let results = self.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
        first_result(results)
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
    /// returned; `limit` counts matching changes, and when it is reached
    /// `last_seq` is the sequence of the last change returned. An invalid
    /// selector returns `BadRequest`.
    pub async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
        let Some(ref selector) = opts.selector else {
            return self.adapter.changes(opts).await;
        };
        let selector = CompiledSelector::new(selector)?;
        let user_wants_docs = opts.include_docs;
        let limit = opts.limit;
        let since = opts.since.clone();
        let mut fetch_opts = ChangesOptions {
            include_docs: true,
            selector: None, // Don't pass to adapter
            ..opts
        };
        // The limit applies after filtering, so read in batches until it is
        // reached (descending feeds cannot be resumed, so read them whole).
        fetch_opts.limit = match limit {
            Some(l) if !fetch_opts.descending => Some(l.max(SELECTOR_CHANGES_BATCH)),
            _ => None,
        };

        let mut results = Vec::new();
        let last_seq = loop {
            let response = self.adapter.changes(fetch_opts.clone()).await?;
            let fetched = response.results.len() as u64;
            for event in response.results {
                if limit.is_some_and(|l| results.len() as u64 >= l) {
                    break;
                }
                if event.doc.as_ref().is_some_and(|d| selector.matches(d)) {
                    results.push(event);
                }
            }
            if limit.is_some_and(|l| results.len() as u64 >= l) {
                break results
                    .last()
                    .map_or(since, |event: &ChangeEvent| event.seq.clone());
            }
            match fetch_opts.limit {
                Some(batch) if fetched >= batch => fetch_opts.since = response.last_seq,
                _ => break response.last_seq,
            }
        };

        if !user_wants_docs {
            for event in &mut results {
                event.doc = None;
            }
        }
        Ok(ChangesResponse { results, last_seq })
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
            // Push the selector into the stream's filter so `limit` counts only
            // matching changes (composing with any pre-existing filter).
            let existing = opts.filter.clone();
            let filter: ChangesFilter = Arc::new(move |e: &ChangeEvent| {
                if let Some(ref ex) = existing
                    && !ex(e)
                {
                    return false;
                }
                e.doc
                    .as_ref()
                    .is_some_and(|d| matches_selector(d, &selector))
            });
            let inner_opts = ChangesStreamOptions {
                include_docs: true, // Need docs for selector evaluation
                selector: None,
                filter: Some(filter),
                ..opts
            };
            let (inner_rx, handle) = live_changes(self.adapter.clone(), inner_opts);
            if user_wants_docs {
                return (inner_rx, handle);
            }

            // Strip docs the user did not request.
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            tokio::spawn(async move {
                let mut inner_rx = inner_rx;
                while let Some(mut event) = inner_rx.recv().await {
                    event.doc = None;
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
            // Push the selector into the stream's filter so `limit` counts only
            // matching changes (composing with any pre-existing filter).
            let existing = opts.filter.clone();
            let filter: ChangesFilter = Arc::new(move |e: &ChangeEvent| {
                if let Some(ref ex) = existing
                    && !ex(e)
                {
                    return false;
                }
                e.doc
                    .as_ref()
                    .is_some_and(|d| matches_selector(d, &selector))
            });
            let inner_opts = ChangesStreamOptions {
                include_docs: true,
                selector: None,
                filter: Some(filter),
                ..opts
            };
            let (inner_rx, handle) = live_changes_events(self.adapter.clone(), inner_opts);
            if user_wants_docs {
                return (inner_rx, handle);
            }

            // Strip docs the user did not request from Change events.
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            tokio::spawn(async move {
                let mut inner_rx = inner_rx;
                while let Some(event) = inner_rx.recv().await {
                    let forward = match event {
                        ChangesEvent::Change(mut ce) => {
                            ce.doc = None;
                            ChangesEvent::Change(ce)
                        }
                        other => other,
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
    /// documents. The index is brought up to date incrementally from the
    /// changes feed; invalid selectors or sort fields return `BadRequest`.
    pub async fn find(&self, opts: FindOptions) -> Result<FindResponse> {
        // Validate the query before doing any work.
        CompiledSelector::new(&opts.selector)?;
        for sort_field in opts.sort.iter().flatten() {
            sort_field.try_field_and_direction()?;
        }

        let usable = usable_index(&*self.indexes.read().await, &opts.selector);
        let candidate_ids = match usable {
            Some(name) => self.index_candidates(&name, &opts.selector).await?,
            None => None,
        };
        let Some(candidate_ids) = candidate_ids else {
            // No usable index — full table scan
            return find(self.adapter.as_ref(), opts).await;
        };

        // Fetch only the candidate docs
        let all = self
            .adapter
            .all_docs(AllDocsOptions {
                include_docs: true,
                keys: Some(candidate_ids),
                ..AllDocsOptions::new()
            })
            .await?;
        find_in_docs(all.rows.into_iter().filter_map(|row| row.doc), &opts)
    }

    /// Bring index `name` up to date with the changes feed and return the
    /// ids of the documents that may match `selector`, or `None` if the index
    /// no longer exists.
    ///
    /// Changes are read without holding the lock; the write lock is only
    /// taken to apply them.
    async fn index_candidates(
        &self,
        name: &str,
        selector: &serde_json::Value,
    ) -> Result<Option<Vec<String>>> {
        loop {
            let since = match self.indexes.read().await.get(name) {
                Some(index) => index.last_seq.clone(),
                None => return Ok(None),
            };
            let changes = self
                .adapter
                .changes(ChangesOptions {
                    since: since.clone(),
                    include_docs: true,
                    ..Default::default()
                })
                .await?;

            if changes.results.is_empty() {
                let indexes = self.indexes.read().await;
                return Ok(indexes
                    .get(name)
                    .map(|index| index.built.find_matching(selector)));
            }

            let mut indexes = self.indexes.write().await;
            let Some(index) = indexes.get_mut(name) else {
                return Ok(None);
            };
            // Another query applied changes meanwhile: catch up from there.
            if index.last_seq != since {
                continue;
            }
            index.built.apply_changes(&changes.results);
            index.last_seq = changes.last_seq;
            return Ok(Some(index.built.find_matching(selector)));
        }
    }

    // -----------------------------------------------------------------
    // Index operations
    // -----------------------------------------------------------------

    /// Create a Mango index for faster queries.
    ///
    /// Equivalent to PouchDB's `db.createIndex()`. Builds the index
    /// immediately by scanning all documents; later finds keep it up to date
    /// from the changes feed.
    pub async fn create_index(&self, def: IndexDefinition) -> Result<CreateIndexResponse> {
        for sort_field in &def.fields {
            sort_field.try_field_and_direction()?;
        }
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

        let exists = || CreateIndexResponse {
            result: "exists".to_string(),
            name: name.clone(),
        };
        if self.indexes.read().await.contains_key(&name) {
            return Ok(exists());
        }

        let index_def = IndexDefinition {
            name: name.clone(),
            fields: def.fields,
            ddoc: def.ddoc,
        };

        // Build from the changes feed, so the index knows the sequence it is
        // up to date with.
        let changes = self
            .adapter
            .changes(ChangesOptions {
                include_docs: true,
                ..Default::default()
            })
            .await?;
        let mut built = BuiltIndex {
            def: index_def,
            entries: Vec::new(),
        };
        built.apply_changes(&changes.results);

        let mut indexes = self.indexes.write().await;
        if indexes.contains_key(&name) {
            return Ok(exists());
        }
        indexes.insert(
            name.clone(),
            MangoIndex {
                built,
                last_seq: changes.last_seq,
            },
        );

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
                name: idx.built.def.name.clone(),
                ddoc: idx.built.def.ddoc.clone(),
                def: IndexFields {
                    fields: idx.built.def.fields.clone(),
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
        let usable = {
            let indexes = self.indexes.read().await;
            usable_index(&indexes, &opts.selector)
                .and_then(|name| indexes.get(&name))
                .map(|index| index.built.def.clone())
        };

        let dbname = self.info().await.map(|i| i.db_name).unwrap_or_default();

        if let Some(def) = usable {
            ExplainResponse {
                dbname,
                index: ExplainIndex {
                    ddoc: def.ddoc,
                    name: def.name,
                    index_type: "json".into(),
                    def: IndexFields { fields: def.fields },
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
    ///
    /// `DesignDocument` only models JavaScript views and a few fields. When
    /// updating (`ddoc.rev` is set), everything else in the revision being
    /// replaced (`views.lib`, Mango index views, view and ddoc `options`,
    /// custom fields) is carried over, so a `get_design` + `put_design`
    /// round trip does not drop it.
    pub async fn put_design(&self, ddoc: DesignDocument) -> Result<DocResult> {
        let mut json = ddoc.to_json();
        if let Some(ref rev) = ddoc.rev {
            let opts = GetOptions {
                rev: Some(rev.clone()),
                ..Default::default()
            };
            match self.adapter.get(&ddoc.id, opts).await {
                Ok(parent) => keep_unmodeled_design_fields(&mut json, &parent.to_json()),
                // A missing or stale revision is reported by the write.
                Err(RouchError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let doc = Document::from_json(json)?;
        let results = self.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
        first_result(results)
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
    /// This is a no-op: a `Database` keeps no view indexes (persistent views
    /// live in a `ViewEngine`, which drops unused ones with
    /// `ViewEngine::remove_indexes_not_in`, and Mango indexes are removed
    /// with `delete_index`). It exists for PouchDB API compatibility.
    pub async fn view_cleanup(&self) -> Result<()> {
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

    /// Destroy the database and all its data, including its Mango indexes.
    pub async fn destroy(&self) -> Result<()> {
        for plugin in &self.plugins {
            plugin.on_destroy().await?;
        }
        self.adapter.destroy().await?;
        self.indexes.write().await.clear();
        Ok(())
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

/// Extract the single `DocResult` from a one-document `bulk_docs` call.
///
/// Returns an error instead of panicking when no result is produced — e.g. a
/// `before_write` plugin dropped the document, or a custom adapter returned
/// fewer results than documents.
fn first_result(results: Vec<DocResult>) -> Result<DocResult> {
    results.into_iter().next().ok_or_else(|| {
        RouchError::DatabaseError("bulk_docs returned no result for the written document".into())
    })
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
    ///
    /// Key ranges are clamped to the partition, and `key`/`keys` outside it
    /// return no rows.
    pub async fn all_docs(&self, mut opts: AllDocsOptions) -> Result<AllDocsResponse> {
        let prefix = format!("{}:", self.name);
        // Every id of the partition sorts between these (byte order).
        let first = prefix.clone();
        let last = format!("{}:{}", self.name, char::MAX);

        if let Some(ref mut keys) = opts.keys {
            keys.retain(|k| k.starts_with(&prefix));
        }
        if opts.key.as_ref().is_some_and(|k| !k.starts_with(&prefix)) {
            opts.key = None;
            opts.keys = Some(Vec::new());
        }

        // The start key is the upper bound when descending.
        let (low, high) = if opts.descending {
            (&mut opts.end_key, &mut opts.start_key)
        } else {
            (&mut opts.start_key, &mut opts.end_key)
        };
        *low = Some(low.take().map_or(first.clone(), |k| k.max(first)));
        *high = Some(high.take().map_or(last.clone(), |k| k.min(last)));

        let mut response = self.db.all_docs(opts).await?;
        response.rows.retain(|row| row.id.starts_with(&prefix));
        Ok(response)
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

/// Batch size for reading a changes feed filtered by a selector.
const SELECTOR_CHANGES_BATCH: u64 = 500;

/// Fields of a design document that `DesignDocument` models.
const MODELED_DESIGN_FIELDS: [&str; 9] = [
    "_id",
    "_rev",
    "views",
    "filters",
    "validate_doc_update",
    "shows",
    "lists",
    "updates",
    "language",
];

/// Copy into `new` (a serialized `DesignDocument`) what `DesignDocument`
/// cannot represent from the `parent` revision: unknown top-level fields,
/// views it skips (`lib`, Mango indexes with a non-string `map`) and extra
/// fields of view definitions (such as `options`).
fn keep_unmodeled_design_fields(new: &mut serde_json::Value, parent: &serde_json::Value) {
    let (Some(new), Some(parent)) = (new.as_object_mut(), parent.as_object()) else {
        return;
    };
    for (key, value) in parent {
        if !key.starts_with('_') && !MODELED_DESIGN_FIELDS.contains(&key.as_str()) {
            new.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }

    let Some(parent_views) = parent.get("views").and_then(|v| v.as_object()) else {
        return;
    };
    let views = new.entry("views").or_insert_with(|| serde_json::json!({}));
    let Some(views) = views.as_object_mut() else {
        return;
    };
    for (name, def) in parent_views {
        let modeled = name != "lib" && def.get("map").is_some_and(|m| m.is_string());
        match views.get_mut(name) {
            // Extra fields of a view that is still defined.
            Some(serde_json::Value::Object(view)) if modeled => {
                for (field, value) in def.as_object().into_iter().flatten() {
                    if field != "map" && field != "reduce" {
                        view.entry(field.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
            // A view removed through the struct stays removed.
            _ if modeled => {}
            _ => {
                views.entry(name.clone()).or_insert_with(|| def.clone());
            }
        }
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
                        // No changes — check whether the doc was replicated.
                        Some(ReplicationEvent::Paused) if remote.get("doc1").await.is_ok() => {
                            got_complete = true;
                            break;
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

    struct DropAllPlugin;

    #[async_trait::async_trait]
    impl Plugin for DropAllPlugin {
        fn name(&self) -> &str {
            "drop-all"
        }
        async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
            docs.clear(); // simulate a plugin that vetoes the write
            Ok(())
        }
    }

    #[tokio::test]
    async fn put_returns_error_when_plugin_drops_document() {
        let db = Database::memory("test").with_plugin(Arc::new(DropAllPlugin));
        // Must return an error rather than panicking on an empty results vec.
        let result = db.put("doc1", serde_json::json!({"v": 1})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn indexed_find_sorts_on_nested_field() {
        let db = Database::memory("test");
        db.put(
            "a",
            serde_json::json!({"category": "x", "address": {"city": "Zurich"}}),
        )
        .await
        .unwrap();
        db.put(
            "b",
            serde_json::json!({"category": "x", "address": {"city": "Amsterdam"}}),
        )
        .await
        .unwrap();
        db.put(
            "c",
            serde_json::json!({"category": "x", "address": {"city": "Madrid"}}),
        )
        .await
        .unwrap();

        // Index on "category" so the index-accelerated find() path is taken.
        db.create_index(IndexDefinition {
            name: String::new(),
            fields: vec![SortField::Simple("category".into())],
            ddoc: None,
        })
        .await
        .unwrap();

        let result = db
            .find(FindOptions {
                selector: serde_json::json!({"category": "x"}),
                sort: Some(vec![SortField::Simple("address.city".into())]),
                ..Default::default()
            })
            .await
            .unwrap();

        let cities: Vec<&str> = result
            .docs
            .iter()
            .map(|d| d["address"]["city"].as_str().unwrap())
            .collect();
        assert_eq!(cities, vec!["Amsterdam", "Madrid", "Zurich"]);
    }

    /// Adapter wrapper that counts full `all_docs` scans.
    struct CountingAdapter {
        inner: MemoryAdapter,
        full_scans: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Adapter for CountingAdapter {
        async fn info(&self) -> Result<DbInfo> {
            self.inner.info().await
        }
        async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
            self.inner.get(id, opts).await
        }
        async fn bulk_docs(
            &self,
            docs: Vec<Document>,
            opts: BulkDocsOptions,
        ) -> Result<Vec<DocResult>> {
            self.inner.bulk_docs(docs, opts).await
        }
        async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
            if opts.keys.is_none() && opts.key.is_none() {
                self.full_scans
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.all_docs(opts).await
        }
        async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
            self.inner.changes(opts).await
        }
        async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
            self.inner.revs_diff(revs).await
        }
        async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
            self.inner.bulk_get(docs).await
        }
        async fn put_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            rev: &str,
            data: Vec<u8>,
            content_type: &str,
        ) -> Result<DocResult> {
            self.inner
                .put_attachment(doc_id, att_id, rev, data, content_type)
                .await
        }
        async fn get_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            opts: GetAttachmentOptions,
        ) -> Result<Vec<u8>> {
            self.inner.get_attachment(doc_id, att_id, opts).await
        }
        async fn remove_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            rev: &str,
        ) -> Result<DocResult> {
            self.inner.remove_attachment(doc_id, att_id, rev).await
        }
        async fn get_local(&self, id: &str) -> Result<serde_json::Value> {
            self.inner.get_local(id).await
        }
        async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
            self.inner.put_local(id, doc).await
        }
        async fn remove_local(&self, id: &str) -> Result<()> {
            self.inner.remove_local(id).await
        }
        async fn compact(&self) -> Result<()> {
            self.inner.compact().await
        }
        async fn destroy(&self) -> Result<()> {
            self.inner.destroy().await
        }
    }

    #[tokio::test]
    async fn indexed_find_updates_index_incrementally() {
        // F47: an indexed find must not rebuild the index (a full scan of
        // every document) on each query, and must still see every write.
        let adapter = Arc::new(CountingAdapter {
            inner: MemoryAdapter::new("test"),
            full_scans: std::sync::atomic::AtomicUsize::new(0),
        });
        let db = Database::from_adapter(adapter.clone());
        for i in 0..20 {
            db.put(&format!("d{i:02}"), serde_json::json!({"age": i}))
                .await
                .unwrap();
        }
        db.create_index(IndexDefinition {
            name: String::new(),
            fields: vec![SortField::Simple("age".into())],
            ddoc: None,
        })
        .await
        .unwrap();
        let scans = || adapter.full_scans.load(std::sync::atomic::Ordering::SeqCst);
        let before = scans();

        let find = |selector: serde_json::Value| {
            let db = &db;
            async move {
                let mut ids: Vec<String> = db
                    .find(FindOptions {
                        selector,
                        ..Default::default()
                    })
                    .await
                    .unwrap()
                    .docs
                    .iter()
                    .map(|d| d["_id"].as_str().unwrap().to_string())
                    .collect();
                ids.sort();
                ids
            }
        };
        assert_eq!(
            find(serde_json::json!({"age": {"$gte": 18}})).await,
            ["d18", "d19"]
        );

        // Writes after the index was built are picked up.
        let rev = db.get("d19").await.unwrap().rev.unwrap().to_string();
        db.update("d19", &rev, serde_json::json!({"age": 1}))
            .await
            .unwrap();
        let rev = db.get("d18").await.unwrap().rev.unwrap().to_string();
        db.remove("d18", &rev).await.unwrap();
        db.put("new", serde_json::json!({"age": 50})).await.unwrap();
        assert_eq!(
            find(serde_json::json!({"age": {"$gte": 18}})).await,
            ["new"]
        );
        assert_eq!(find(serde_json::json!({"age": 1})).await, ["d01", "d19"]);
        assert_eq!(find(serde_json::json!({"age": {"$lt": 1}})).await, ["d00"]);

        assert_eq!(
            scans(),
            before,
            "indexed finds must not rescan every document"
        );
    }

    #[tokio::test]
    async fn indexed_find_matches_full_scan() {
        // F47: the index only narrows candidates; results must be the same
        // as a full scan for any selector touching the indexed field.
        let db = Database::memory("test");
        let values = [
            serde_json::json!(null),
            serde_json::json!(1),
            serde_json::json!(2.5),
            serde_json::json!(-3),
            serde_json::json!("a"),
            serde_json::json!("b"),
            serde_json::json!([1, 2]),
            serde_json::json!({"x": 1}),
        ];
        for (i, v) in values.iter().enumerate() {
            db.put(&format!("v{i}"), serde_json::json!({"f": v, "g": i}))
                .await
                .unwrap();
        }
        db.put("missing", serde_json::json!({"g": 99}))
            .await
            .unwrap();
        db.create_index(IndexDefinition {
            name: "by-f".into(),
            fields: vec![SortField::Simple("f".into())],
            ddoc: None,
        })
        .await
        .unwrap();

        let mut selectors = vec![
            serde_json::json!({"f": {"$exists": false}}),
            serde_json::json!({"f": {"$ne": 1}}),
            serde_json::json!({"f": {"$in": [1, "a"]}}),
            serde_json::json!({"f": {"x": 1}}),
            serde_json::json!({"f": {"$gt": 1, "$lt": "b"}}),
            serde_json::json!({"f": {"$type": "array"}}),
        ];
        for v in &values {
            for op in ["$eq", "$gt", "$gte", "$lt", "$lte"] {
                selectors.push(serde_json::json!({"f": {op: v}}));
            }
            selectors.push(serde_json::json!({"f": v}));
        }
        for selector in selectors {
            let opts = FindOptions {
                selector: selector.clone(),
                sort: Some(vec![SortField::Simple("g".into())]),
                ..Default::default()
            };
            let indexed = db.find(opts.clone()).await.unwrap().docs;
            let scanned = find(db.adapter(), opts).await.unwrap().docs;
            assert_eq!(indexed, scanned, "{selector}");
        }
    }

    #[tokio::test]
    async fn concurrent_indexed_finds_see_all_writes() {
        let db = Arc::new(Database::memory("test"));
        db.create_index(IndexDefinition {
            name: String::new(),
            fields: vec![SortField::Simple("n".into())],
            ddoc: None,
        })
        .await
        .unwrap();
        let mut tasks = Vec::new();
        for t in 0..4 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                for i in 0..25 {
                    db.put(&format!("t{t}-{i}"), serde_json::json!({"n": i}))
                        .await
                        .unwrap();
                    db.find(FindOptions {
                        selector: serde_json::json!({"n": {"$gte": 0}}),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let all = db
            .find(FindOptions {
                selector: serde_json::json!({"n": {"$gte": 0}}),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(all.docs.len(), 100);
    }

    #[tokio::test]
    async fn changes_with_selector_applies_limit_after_filtering() {
        // F58: limit counts matching changes, and last_seq is the seq of the
        // last one returned.
        let db = Database::memory("test");
        for i in 0..10 {
            db.put(&format!("b{i}"), serde_json::json!({"type": "b"}))
                .await
                .unwrap();
        }
        for i in 0..10 {
            db.put(&format!("a{i}"), serde_json::json!({"type": "a"}))
                .await
                .unwrap();
        }
        let changes = db
            .changes(ChangesOptions {
                selector: Some(serde_json::json!({"type": "a"})),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        let ids: Vec<_> = changes.results.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a0", "a1", "a2", "a3", "a4"]);
        assert_eq!(changes.last_seq, Seq::Num(15));

        // Continuing from last_seq returns the rest.
        let rest = db
            .changes(ChangesOptions {
                since: changes.last_seq,
                selector: Some(serde_json::json!({"type": "a"})),
                limit: Some(100),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rest.results.len(), 5);
        assert_eq!(rest.last_seq, Seq::Num(20));

        // Invalid selectors are rejected.
        assert!(
            db.changes(ChangesOptions {
                selector: Some(serde_json::json!({"type": {"$regex": "["}})),
                ..Default::default()
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn partition_all_docs_is_scoped_to_the_partition() {
        // F60: descending, key/keys and astral ids stay inside the partition.
        let db = Database::memory("test");
        for id in [
            "orders:1",
            "users:1",
            "users:2",
            "users:\u{1F600}",
            "usersx:1",
        ] {
            db.put(id, serde_json::json!({})).await.unwrap();
        }
        let users = db.partition("users");
        let ids = |r: AllDocsResponse| r.rows.into_iter().map(|r| r.id).collect::<Vec<_>>();

        let all = ids(users.all_docs(AllDocsOptions::new()).await.unwrap());
        assert_eq!(all, ["users:1", "users:2", "users:\u{1F600}"]);

        let desc = ids(users
            .all_docs(AllDocsOptions {
                descending: true,
                ..AllDocsOptions::new()
            })
            .await
            .unwrap());
        assert_eq!(desc, ["users:\u{1F600}", "users:2", "users:1"]);

        let keyed = ids(users
            .all_docs(AllDocsOptions {
                keys: Some(vec!["orders:1".into(), "users:2".into()]),
                ..AllDocsOptions::new()
            })
            .await
            .unwrap());
        assert_eq!(keyed, ["users:2"]);

        let foreign = ids(users
            .all_docs(AllDocsOptions {
                key: Some("orders:1".into()),
                ..AllDocsOptions::new()
            })
            .await
            .unwrap());
        assert!(foreign.is_empty());

        // A range reaching outside the partition is clamped to it.
        let clamped = ids(users
            .all_docs(AllDocsOptions {
                start_key: Some("a".into()),
                end_key: Some("users:1".into()),
                ..AllDocsOptions::new()
            })
            .await
            .unwrap());
        assert_eq!(clamped, ["users:1"]);
    }
}
