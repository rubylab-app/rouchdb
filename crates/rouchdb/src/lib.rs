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

// The README's Rust examples are compiled (and run) as doctests of this
// crate, so they cannot drift from the API.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct ReadmeDoctests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::RwLock;

// Re-export core types
pub use rouchdb_core::adapter::Adapter;
pub use rouchdb_core::document::*;
pub use rouchdb_core::error::{Result, RouchError};
pub use rouchdb_core::json::MAX_NESTING_DEPTH;
pub use rouchdb_core::merge::{is_deleted, winning_rev};

// Re-export adapters
pub use rouchdb_adapter_http::auth::{AuthClient, Session, UserContext};
pub use rouchdb_adapter_http::{HttpAdapter, HttpAdapterOptions};
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
/// Plugins receive lifecycle hooks during database operations, including
/// documents replicated into the database.
#[async_trait::async_trait]
pub trait Plugin: Send + Sync {
    /// The plugin name.
    fn name(&self) -> &str;
    /// Called before documents are written; may modify them or reject the
    /// write with an error.
    ///
    /// For replicated documents it acts like a CouchDB `validate_doc_update`:
    /// it is called once per document, changes it makes are ignored (the
    /// source's body is kept under the source's revision id), and a document
    /// it drops or rejects with `Forbidden`, `Unauthorized` or `BadRequest`
    /// is reported as denied while the rest of the batch is still written.
    ///
    /// `put_attachment` and `remove_attachment` call it with the revision
    /// they create: the parent's body and its attachments with the change
    /// applied (a new attachment carries its data). It can reject the write
    /// (the error is returned unchanged, and dropping the document is
    /// `Forbidden`), but its changes are ignored: the stored body already
    /// went through the plugins when it was written.
    async fn before_write(&self, _docs: &mut Vec<Document>) -> Result<()> {
        Ok(())
    }
    /// Called after documents (or attachments) were written. The write is
    /// already committed: an error is returned to the caller but does not
    /// undo it, so retrying the same write will conflict.
    async fn after_write(&self, _results: &[DocResult]) -> Result<()> {
        Ok(())
    }
    /// Called when the database is destroyed.
    async fn on_destroy(&self) -> Result<()> {
        Ok(())
    }
}

/// Runs a database's plugins around writes that do not go through
/// `Database::bulk_docs`: it is the adapter replication writes into.
struct PluginAdapter {
    inner: Arc<dyn Adapter>,
    plugins: Vec<Arc<dyn Plugin>>,
}

/// The per-doc error a plugin rejection is reported as, if it is a
/// rejection that retrying cannot fix.
fn denial(error: &RouchError) -> Option<&'static str> {
    match error {
        RouchError::Forbidden(_) | RouchError::BadRequest(_) => Some("forbidden"),
        RouchError::Unauthorized => Some("unauthorized"),
        _ => None,
    }
}

impl PluginAdapter {
    /// Run `before_write` on the revision an attachment write creates: the
    /// document at `rev` with `edit` applied to its attachments (a document
    /// with no body when `rev` cannot be read, which the write itself then
    /// reports). As for replicated documents, plugins only accept or reject
    /// it: the stored body already went through them, so their changes are
    /// not applied. A rejection is returned as is; dropping the document is
    /// `Forbidden`.
    async fn validate_attachment_edit(
        &self,
        doc_id: &str,
        rev: &str,
        edit: impl FnOnce(&mut HashMap<String, AttachmentMeta>),
    ) -> Result<()> {
        let opts = GetOptions {
            rev: Some(rev.to_string()),
            ..Default::default()
        };
        let mut doc = match self.inner.get(doc_id, opts).await {
            Ok(doc) => doc,
            Err(_) => Document {
                id: doc_id.to_string(),
                rev: rev.parse().ok(),
                deleted: false,
                data: serde_json::json!({}),
                attachments: HashMap::new(),
            },
        };
        doc.deleted = false;
        edit(&mut doc.attachments);
        let mut docs = vec![doc];
        for plugin in &self.plugins {
            plugin.before_write(&mut docs).await?;
            if docs.is_empty() {
                return Err(RouchError::Forbidden(format!(
                    "dropped by plugin {}",
                    plugin.name()
                )));
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Adapter for PluginAdapter {
    async fn info(&self) -> Result<DbInfo> {
        self.inner.info().await
    }

    async fn id(&self) -> Result<String> {
        self.inner.id().await
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        self.inner.get(id, opts).await
    }

    async fn bulk_docs(
        &self,
        docs: Vec<Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>> {
        // Validate each doc on its own so one rejected doc is reported as
        // denied instead of failing (and forever blocking) the whole batch.
        // Plugins only accept or reject replicated docs: the original body
        // is written, since a changed body stored under the source's
        // revision id would silently diverge from it for good.
        let mut accepted = Vec::with_capacity(docs.len());
        let mut denied = Vec::new();
        for doc in docs {
            let mut probe = vec![doc.clone()];
            let mut outcome = Ok(());
            for plugin in &self.plugins {
                outcome = plugin.before_write(&mut probe).await;
                if outcome.is_err() {
                    break;
                }
                if probe.is_empty() {
                    outcome = Err(RouchError::Forbidden(format!(
                        "dropped by plugin {}",
                        plugin.name()
                    )));
                    break;
                }
            }
            match outcome {
                Ok(()) => accepted.push(doc),
                Err(e) => match denial(&e) {
                    Some(kind) => denied.push(DocResult {
                        ok: false,
                        id: doc.id,
                        rev: None,
                        error: Some(kind.to_string()),
                        reason: Some(e.to_string()),
                    }),
                    None => return Err(e),
                },
            }
        }

        let written: Vec<(String, Option<String>)> = accepted
            .iter()
            .map(|d| (d.id.clone(), d.rev.as_ref().map(|r| r.to_string())))
            .collect();
        let mut results = if accepted.is_empty() {
            Vec::new()
        } else {
            self.inner.bulk_docs(accepted, opts.clone()).await?
        };
        // CouchDB answers new_edits=false writes with only the failures;
        // report the rest as written so after_write sees every doc.
        if !opts.new_edits && results.len() < written.len() {
            let failed: std::collections::HashSet<String> =
                results.iter().map(|r| r.id.clone()).collect();
            for (id, rev) in written {
                if !failed.contains(&id) {
                    results.push(DocResult {
                        ok: true,
                        id,
                        rev,
                        error: None,
                        reason: None,
                    });
                }
            }
        }
        for plugin in &self.plugins {
            plugin.after_write(&results).await?;
        }
        results.extend(denied);
        Ok(results)
    }

    async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
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
        let attachment = AttachmentMeta {
            content_type: content_type.to_string(),
            digest: rouchdb_core::document::attachment_digest(&data),
            length: data.len() as u64,
            stub: false,
            data: Some(data.clone()),
        };
        self.validate_attachment_edit(doc_id, rev, |atts| {
            atts.insert(att_id.to_string(), attachment);
        })
        .await?;
        let result = self
            .inner
            .put_attachment(doc_id, att_id, rev, data, content_type)
            .await?;
        for plugin in &self.plugins {
            plugin.after_write(std::slice::from_ref(&result)).await?;
        }
        Ok(result)
    }

    async fn get_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>> {
        self.inner.get_attachment(doc_id, att_id, opts).await
    }

    async fn remove_attachment(&self, doc_id: &str, att_id: &str, rev: &str) -> Result<DocResult> {
        self.validate_attachment_edit(doc_id, rev, |atts| {
            atts.remove(att_id);
        })
        .await?;
        let result = self.inner.remove_attachment(doc_id, att_id, rev).await?;
        for plugin in &self.plugins {
            plugin.after_write(std::slice::from_ref(&result)).await?;
        }
        Ok(result)
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

    async fn close(&self) -> Result<()> {
        self.inner.close().await
    }

    async fn purge(&self, req: HashMap<String, Vec<String>>) -> Result<PurgeResponse> {
        self.inner.purge(req).await
    }

    async fn get_security(&self) -> Result<SecurityDocument> {
        self.inner.get_security().await
    }

    async fn put_security(&self, doc: SecurityDocument) -> Result<()> {
        self.inner.put_security(doc).await
    }
}

/// A high-level database handle that wraps any adapter implementation.
///
/// Provides a user-friendly API similar to PouchDB's JavaScript interface.
pub struct Database {
    adapter: Arc<dyn Adapter>,
    /// Set for `http()` databases, whose Mango queries and indexes run on
    /// the server.
    remote: Option<Arc<HttpAdapter>>,
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
            remote: None,
            indexes: Arc::new(RwLock::new(HashMap::new())),
            plugins: Vec::new(),
        }
    }

    /// Open or create a persistent database backed by redb.
    pub fn open(path: impl AsRef<Path>, name: &str) -> Result<Self> {
        let adapter = RedbAdapter::open(path, name)?;
        Ok(Self {
            adapter: Arc::new(adapter),
            remote: None,
            indexes: Arc::new(RwLock::new(HashMap::new())),
            plugins: Vec::new(),
        })
    }

    /// Connect to a remote CouchDB instance.
    ///
    /// Mango queries and indexes (`find`, `create_index`, `get_indexes`,
    /// `delete_index`, `explain`) run on the server.
    pub fn http(url: &str) -> Self {
        Self::remote(HttpAdapter::new(url))
    }

    /// Connect to a remote CouchDB instance using an authenticated client.
    ///
    /// The `AuthClient` should have been logged in via `auth.login()` first.
    pub fn http_with_auth(url: &str, auth: &AuthClient) -> Self {
        Self::remote(HttpAdapter::with_auth_client(url, auth))
    }

    fn remote(adapter: HttpAdapter) -> Self {
        let adapter = Arc::new(adapter);
        Self {
            adapter: adapter.clone(),
            remote: Some(adapter),
            indexes: Arc::new(RwLock::new(HashMap::new())),
            plugins: Vec::new(),
        }
    }

    /// Create a database from any adapter implementation.
    pub fn from_adapter(adapter: Arc<dyn Adapter>) -> Self {
        Self {
            adapter,
            remote: None,
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

    /// The adapter writes from outside `bulk_docs` (replication, attachment
    /// updates) go through, so they run this database's plugins.
    fn plugin_adapter(&self) -> Arc<dyn Adapter> {
        if self.plugins.is_empty() {
            self.adapter.clone()
        } else {
            Arc::new(PluginAdapter {
                inner: self.adapter.clone(),
                plugins: self.plugins.clone(),
            })
        }
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
    /// Equivalent to PouchDB's `db.post(doc)`. Uses the body's `_id` when it
    /// has one, otherwise generates a UUID v4.
    pub async fn post(&self, data: serde_json::Value) -> Result<DocResult> {
        let mut doc = Document {
            id: String::new(),
            rev: None,
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        doc.prepare_for_write()?;
        if doc.id.is_empty() {
            doc.id = uuid::Uuid::new_v4().to_string();
        }
        self.write_one(doc).await
    }

    /// Create or update a document.
    ///
    /// If the document doesn't exist, creates it. To update an existing
    /// document, include its current `_rev` in `data` (or use `update`).
    /// Special members in `data` are interpreted like CouchDB does
    /// (`_rev`, `_deleted`, `_attachments`); read-only metadata such as
    /// `_conflicts` is ignored and any other `_`-prefixed member is rejected.
    ///
    /// A failed write (conflict, invalid document, ...) is returned as an
    /// error, never as `Ok` with `ok: false`.
    pub async fn put(&self, id: &str, data: serde_json::Value) -> Result<DocResult> {
        if id.is_empty() {
            return Err(RouchError::MissingId);
        }
        let mut doc = Document {
            id: id.to_string(),
            rev: None,
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        doc.prepare_for_write()?;
        self.write_one(doc).await
    }

    /// Update an existing document (requires providing the current rev).
    ///
    /// Returns `RouchError::Conflict` if `rev` is not a current leaf
    /// revision of the document.
    pub async fn update(&self, id: &str, rev: &str, data: serde_json::Value) -> Result<DocResult> {
        if id.is_empty() {
            return Err(RouchError::MissingId);
        }
        let revision: Revision = rev.parse()?;
        let mut doc = Document {
            id: id.to_string(),
            rev: Some(revision),
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        doc.prepare_for_write()?;
        self.write_one(doc).await
    }

    /// Delete a document (requires the current rev).
    ///
    /// Like CouchDB's `DELETE` (and PouchDB's `remove`), a document that
    /// does not exist or is already deleted is `RouchError::NotFound`;
    /// `RouchError::Conflict` if `rev` is not a current leaf revision of the
    /// document.
    pub async fn remove(&self, id: &str, rev: &str) -> Result<DocResult> {
        if id.is_empty() {
            return Err(RouchError::MissingId);
        }
        let revision: Revision = rev.parse()?;
        self.ensure_live(id).await?;
        let doc = Document {
            id: id.to_string(),
            rev: Some(revision),
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        self.write_one(doc).await
    }

    /// `NotFound` unless `id` is a live document (or an existing local
    /// document), the check CouchDB's `DELETE` makes before writing.
    async fn ensure_live(&self, id: &str) -> Result<()> {
        match rouchdb_core::write::local_doc_id(id) {
            Some(local) => self.adapter.get_local(local).await.map(|_| ()),
            None => self
                .adapter
                .get(id, GetOptions::default())
                .await
                .map(|_| ()),
        }
    }

    /// Write a single document and turn a failed `DocResult` into an error.
    async fn write_one(&self, doc: Document) -> Result<DocResult> {
        let results = self.bulk_docs(vec![doc], BulkDocsOptions::new()).await?;
        let result = first_result(results)?;
        if result.ok {
            Ok(result)
        } else {
            Err(doc_result_error(result))
        }
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
    /// If `opts.selector` is set, only changes whose document matches the
    /// Mango selector are forwarded through the channel.
    pub fn live_changes(
        &self,
        opts: ChangesStreamOptions,
    ) -> (tokio::sync::mpsc::Receiver<ChangeEvent>, ChangesHandle) {
        live_changes(self.adapter.clone(), opts)
    }

    /// Start a live changes feed with lifecycle events.
    ///
    /// Like `live_changes()` but returns `ChangesEvent` which includes
    /// `Active`, `Paused`, `Complete`, and `Error` in addition to `Change`.
    pub fn live_changes_events(
        &self,
        opts: ChangesStreamOptions,
    ) -> (tokio::sync::mpsc::Receiver<ChangesEvent>, ChangesHandle) {
        live_changes_events(self.adapter.clone(), opts)
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
        self.plugin_adapter()
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
        self.plugin_adapter()
            .remove_attachment(doc_id, att_id, rev)
            .await
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
    ///
    /// On an `http()` database the query runs on the server (`_find`); only
    /// when the server has no index for the requested sort are the
    /// documents fetched and queried locally.
    pub async fn find(&self, opts: FindOptions) -> Result<FindResponse> {
        if let Some(ref remote) = self.remote
            && let Some(response) = remote_find(remote, &opts).await?
        {
            return Ok(response);
        }

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
    /// from the changes feed. On an `http()` database the index is created
    /// on the server (`_index`).
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

        if let Some(ref remote) = self.remote {
            return remote_create_index(remote, name, def).await;
        }

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
    ///
    /// On an `http()` database these are the server's JSON indexes (none if
    /// the server cannot be reached).
    pub async fn get_indexes(&self) -> Vec<IndexInfo> {
        if let Some(ref remote) = self.remote {
            let mut result: Vec<IndexInfo> = remote_indexes(remote)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(_, info)| info)
                .collect();
            result.sort_by(|a, b| a.name.cmp(&b.name));
            return result;
        }
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
        if let Some(ref remote) = self.remote
            && let Some(explained) = remote_explain(remote, &opts).await
        {
            return explained;
        }
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
        if let Some(ref remote) = self.remote {
            return remote_delete_index(remote, name).await;
        }
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
    /// Like `put`, a failed write (e.g. `RouchError::Conflict`) is an error,
    /// never `Ok` with `ok: false`.
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
        let mut doc = Document::from_json(json)?;
        doc.prepare_for_write()?;
        self.write_one(doc).await
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
    ///
    /// The target's plugins run on the replicated documents.
    pub async fn replicate_to(&self, target: &Database) -> Result<ReplicationResult> {
        replicate(
            self.adapter.as_ref(),
            target.plugin_adapter().as_ref(),
            ReplicationOptions::default(),
        )
        .await
    }

    /// Replicate from the source to this database.
    ///
    /// This database's plugins run on the replicated documents.
    pub async fn replicate_from(&self, source: &Database) -> Result<ReplicationResult> {
        replicate(
            source.adapter.as_ref(),
            self.plugin_adapter().as_ref(),
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
        replicate(
            self.adapter.as_ref(),
            target.plugin_adapter().as_ref(),
            opts,
        )
        .await
    }

    /// Replicate with event streaming.
    ///
    /// Same as `replicate_to()` but also returns every `ReplicationEvent`
    /// emitted while replicating, buffered in the returned receiver.
    pub async fn replicate_to_with_events(
        &self,
        target: &Database,
        opts: ReplicationOptions,
    ) -> Result<(
        ReplicationResult,
        tokio::sync::mpsc::Receiver<ReplicationEvent>,
    )> {
        // Drain the events while replicating: nobody can read the returned
        // receiver until this call finishes, so a bounded channel that fills
        // up would stall the replication forever.
        let (tx, mut inner_rx) = tokio::sync::mpsc::channel(64);
        let target_adapter = target.plugin_adapter();
        let replication =
            replicate_with_events(self.adapter.as_ref(), target_adapter.as_ref(), opts, tx);
        let collect = async {
            let mut events = Vec::new();
            while let Some(event) = inner_rx.recv().await {
                events.push(event);
            }
            events
        };
        let (result, events) = tokio::join!(replication, collect);
        let result = result?;

        // Hand the events back through a channel large enough to hold them.
        let (tx, rx) = tokio::sync::mpsc::channel(events.len().max(1));
        for event in events {
            let _ = tx.try_send(event);
        }
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
        replicate_live(self.adapter.clone(), target.plugin_adapter(), opts)
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

    /// Destroy the database and all its data: documents, local documents
    /// (replication checkpoints included), attachments, the security
    /// document and its Mango indexes.
    ///
    /// The handle stays usable and then behaves as a new, empty database,
    /// whatever the adapter: memory and redb start over in place, and an
    /// http database is re-created on its next use (unless it was opened
    /// with `skip_setup`, in which case operations fail with `NotFound`
    /// until the database is created again).
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

/// Map a failed per-document write result to the matching error.
fn doc_result_error(result: DocResult) -> RouchError {
    let reason = result
        .reason
        .clone()
        .or_else(|| result.error.clone())
        .unwrap_or_else(|| "document write failed".into());
    match result.error.as_deref() {
        Some("conflict") => RouchError::Conflict,
        Some("not_found") => RouchError::NotFound(result.id),
        Some("unauthorized") => RouchError::Unauthorized,
        Some("forbidden") => RouchError::Forbidden(reason),
        _ => RouchError::BadRequest(reason),
    }
}

/// Restrict an `all_docs` range query to a partition, whose ids are, in id
/// (byte) order, exactly those in `[prefix, after)` (`after` is the prefix
/// with its final `:` bumped to `;`).
///
/// A bound outside the partition is replaced by the partition's own, with
/// the inclusiveness that bound needs: `inclusive_end` stays the caller's
/// only while the end key is the caller's.
///
/// Descending, the upper bound is the start key, which is always inclusive,
/// so a document whose id is `after` itself comes first. The query then
/// asks for one row more without skipping and returns the caller's
/// `(skip, limit)`, to apply once the rows outside the partition are gone.
fn clamp_to_partition(
    opts: &mut AllDocsOptions,
    prefix: &str,
    after: &str,
) -> Option<(u64, Option<u64>)> {
    let mut page = None;
    if opts.descending {
        if !opts.start_key.as_deref().is_some_and(|k| k < after) {
            opts.start_key = Some(after.to_string());
            page = Some((opts.skip, opts.limit));
            opts.limit = opts
                .limit
                .map(|l| l.saturating_add(opts.skip).saturating_add(1));
            opts.skip = 0;
        }
        if !opts.end_key.as_deref().is_some_and(|k| k >= prefix) {
            opts.end_key = Some(prefix.to_string());
            opts.inclusive_end = true;
        }
    } else {
        if !opts.start_key.as_deref().is_some_and(|k| k >= prefix) {
            opts.start_key = Some(prefix.to_string());
        }
        if !opts.end_key.as_deref().is_some_and(|k| k < after) {
            opts.end_key = Some(after.to_string());
            opts.inclusive_end = false;
        }
    }
    page
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

        if let Some(ref mut keys) = opts.keys {
            keys.retain(|k| k.starts_with(&prefix));
        }
        if opts.key.as_ref().is_some_and(|k| !k.starts_with(&prefix)) {
            opts.key = None;
            opts.keys = Some(Vec::new());
        }
        let page = if opts.key.is_none() && opts.keys.is_none() {
            clamp_to_partition(&mut opts, &prefix, &format!("{};", self.name))
        } else {
            None
        };

        let mut response = self.db.all_docs(opts).await?;
        response.rows.retain(|row| row.id.starts_with(&prefix));
        if let Some((skip, limit)) = page {
            let limit = limit.map_or(usize::MAX, |l| l as usize);
            response.rows = response
                .rows
                .into_iter()
                .skip(skip as usize)
                .take(limit)
                .collect();
            response.offset = response.offset.saturating_add(skip);
        }
        Ok(response)
    }

    /// Run a Mango find query scoped to this partition.
    pub async fn find(&self, mut opts: FindOptions) -> Result<FindResponse> {
        // Validate the selector as a whole selector first: in a combinator
        // some invalid ones (`{"$gt": 1}`) would be accepted.
        CompiledSelector::new(&opts.selector)?;
        let escaped = regex_escape(&self.name);
        let partition_filter = serde_json::json!({"_id": {"$regex": format!("^{}:", escaped)}});
        opts.selector = match opts.selector {
            // `{}` matches every document, but inside `$and` it would be an
            // equality test with `{}` that no document passes.
            serde_json::Value::Object(ref map) if map.is_empty() => partition_filter,
            selector => serde_json::json!({"$and": [selector, partition_filter]}),
        };
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
// Mango on a remote CouchDB
// ---------------------------------------------------------------------------

/// `limit` sent to `_find` when the caller wants every match (CouchDB
/// returns 25 documents by default).
const REMOTE_FIND_NO_LIMIT: u64 = (1 << 53) - 1;

/// Turn an error response from CouchDB into a `RouchError`.
fn remote_error(status: u16, body: &serde_json::Value) -> RouchError {
    let reason = body
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or_default()
        .to_string();
    match status {
        400 => RouchError::BadRequest(reason),
        401 => RouchError::Unauthorized,
        403 => RouchError::Forbidden(reason),
        404 => RouchError::NotFound(reason),
        409 => RouchError::Conflict,
        _ => RouchError::DatabaseError(format!("HTTP {status}: {body}")),
    }
}

/// Send a request and return the JSON body of a successful response, or
/// the status and body of an error response.
async fn remote_request(
    remote: &HttpAdapter,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<std::result::Result<serde_json::Value, (u16, serde_json::Value)>> {
    let (status, response) = remote.request_json(method, path, body).await?;
    Ok(if (200..300).contains(&status) {
        Ok(response)
    } else {
        Err((status, response))
    })
}

/// The `_find`/`_explain` request body for a query.
fn remote_query_body(opts: &FindOptions) -> Result<serde_json::Value> {
    let mut body = serde_json::to_value(opts)?;
    body["limit"] = serde_json::json!(opts.limit.unwrap_or(REMOTE_FIND_NO_LIMIT));
    Ok(body)
}

/// Run a query with `_find`; `None` if the server has no index for the
/// requested sort (the caller then queries locally).
async fn remote_find(remote: &HttpAdapter, opts: &FindOptions) -> Result<Option<FindResponse>> {
    let body = remote_query_body(opts)?;
    match remote_request(remote, "POST", "_find", Some(&body)).await? {
        Ok(response) => {
            let docs = match response.get("docs") {
                Some(serde_json::Value::Array(docs)) => docs.clone(),
                _ => Vec::new(),
            };
            Ok(Some(FindResponse { docs }))
        }
        Err((400, body)) if body["error"] == "no_usable_index" => Ok(None),
        Err((status, body)) => Err(remote_error(status, &body)),
    }
}

async fn remote_create_index(
    remote: &HttpAdapter,
    name: String,
    def: IndexDefinition,
) -> Result<CreateIndexResponse> {
    let mut body = serde_json::json!({
        "index": {"fields": def.fields},
        "name": name,
        "type": "json",
    });
    if let Some(ddoc) = def.ddoc {
        body["ddoc"] = serde_json::json!(ddoc);
    }
    match remote_request(remote, "POST", "_index", Some(&body)).await? {
        Ok(response) => Ok(CreateIndexResponse {
            result: response["result"].as_str().unwrap_or("created").to_string(),
            name: response["name"].as_str().unwrap_or(&name).to_string(),
        }),
        Err((status, body)) => Err(remote_error(status, &body)),
    }
}

/// The server's JSON indexes, with the id of their design document.
async fn remote_indexes(remote: &HttpAdapter) -> Result<Vec<(String, IndexInfo)>> {
    let response = remote_request(remote, "GET", "_index", None)
        .await?
        .map_err(|(status, body)| remote_error(status, &body))?;
    let mut result = Vec::new();
    for index in response["indexes"].as_array().into_iter().flatten() {
        if index["type"] != "json" {
            continue;
        }
        let ddoc = index["ddoc"].as_str().unwrap_or_default().to_string();
        let fields = serde_json::from_value(index["def"]["fields"].clone()).unwrap_or_default();
        result.push((
            ddoc.clone(),
            IndexInfo {
                name: index["name"].as_str().unwrap_or_default().to_string(),
                ddoc: Some(ddoc),
                def: IndexFields { fields },
            },
        ));
    }
    Ok(result)
}

async fn remote_delete_index(remote: &HttpAdapter, name: &str) -> Result<()> {
    let ddoc = remote_indexes(remote)
        .await?
        .into_iter()
        .find(|(_, info)| info.name == name)
        .map(|(ddoc, _)| ddoc)
        .ok_or_else(|| RouchError::NotFound(format!("index {}", name)))?;
    let ddoc = ddoc.strip_prefix("_design/").unwrap_or(&ddoc);
    let path = format!(
        "_index/{}/json/{}",
        encode_path_segment(ddoc),
        encode_path_segment(name)
    );
    remote_request(remote, "DELETE", &path, None)
        .await?
        .map(|_| ())
        .map_err(|(status, body)| remote_error(status, &body))
}

/// Ask the server how it would run a query; `None` if that fails.
async fn remote_explain(remote: &HttpAdapter, opts: &FindOptions) -> Option<ExplainResponse> {
    let body = remote_query_body(opts).ok()?;
    let response = remote_request(remote, "POST", "_explain", Some(&body))
        .await
        .ok()?
        .ok()?;
    let index = &response["index"];
    Some(ExplainResponse {
        dbname: response["dbname"].as_str().unwrap_or_default().to_string(),
        index: ExplainIndex {
            ddoc: index["ddoc"].as_str().map(str::to_string),
            name: index["name"].as_str().unwrap_or_default().to_string(),
            index_type: index["type"].as_str().unwrap_or_default().to_string(),
            def: IndexFields {
                fields: serde_json::from_value(index["def"]["fields"].clone()).unwrap_or_default(),
            },
        },
        selector: opts.selector.clone(),
        fields: opts.fields.clone(),
    })
}

/// Percent-encode a URL path segment (everything but RFC 3986 unreserved
/// characters).
fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
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
        assert!(r2.rev.as_deref().unwrap().starts_with("2-"), "{r2:?}");

        assert!(matches!(db.get("doc1").await, Err(RouchError::NotFound(_))));
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
        let uuid = uuid::Uuid::parse_str(&r1.id).expect("post generates a UUID id");
        assert_eq!(uuid.get_version_num(), 4, "{}", r1.id);

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

        // Removing an attachment the document does not have is an error and
        // writes nothing.
        assert!(matches!(
            db.remove_attachment("doc1", "photo.jpg", &rev).await,
            Err(RouchError::NotFound(_))
        ));

        let rev = db
            .put_attachment("doc1", "photo.jpg", &rev, vec![1, 2, 3], "image/jpeg")
            .await
            .unwrap()
            .rev
            .unwrap();
        let r2 = db
            .remove_attachment("doc1", "photo.jpg", &rev)
            .await
            .unwrap();
        assert!(r2.ok);
        assert!(r2.rev.is_some());
        assert_ne!(r2.rev.as_deref().unwrap(), rev);
        assert!(matches!(
            db.get_attachment("doc1", "photo.jpg").await,
            Err(RouchError::NotFound(_))
        ));
        assert!(db.get("doc1").await.unwrap().attachments.is_empty());
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
                sort: Some(vec![SortField::Simple("age".into())]),
                ..Default::default()
            })
            .await
            .unwrap();
        let ids: Vec<&str> = found
            .docs
            .iter()
            .map(|d| d["_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["alice", "charlie"]);

        // Verify get_indexes
        let indexes = db.get_indexes().await;
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name, "idx-age");

        // Delete index
        db.delete_index("idx-age").await.unwrap();
        assert!(matches!(
            db.delete_index("idx-age").await,
            Err(RouchError::NotFound(_))
        ));
        assert!(matches!(
            db.delete_index("nonexistent").await,
            Err(RouchError::NotFound(_))
        ));

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
    async fn replicate_to_with_events_does_not_stall_on_many_events() {
        let local = Database::memory("local");
        let remote = Database::memory("remote");
        for i in 0..100 {
            local
                .put(&format!("doc{i:03}"), serde_json::json!({"i": i}))
                .await
                .unwrap();
        }

        // batch_size=1 produces more events than any fixed channel capacity.
        let replication = local.replicate_to_with_events(
            &remote,
            ReplicationOptions {
                batch_size: 1,
                ..Default::default()
            },
        );
        let (result, mut rx) =
            tokio::time::timeout(std::time::Duration::from_secs(30), replication)
                .await
                .expect("replicate_to_with_events stalled")
                .unwrap();
        assert!(result.ok);
        assert_eq!(result.docs_written, 100);

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let changes = events
            .iter()
            .filter(|e| matches!(e, ReplicationEvent::Change { .. }))
            .count();
        assert_eq!(changes, 100);
        assert!(matches!(events.first(), Some(ReplicationEvent::Active)));
        assert!(matches!(events.last(), Some(ReplicationEvent::Complete(_))));
    }

    /// Waits until `id` is readable on `db`, draining the replication
    /// events meanwhile.
    async fn wait_for_doc(
        db: &Database,
        id: &str,
        rx: &mut tokio::sync::mpsc::Receiver<ReplicationEvent>,
    ) -> Document {
        let wait = async {
            loop {
                while rx.try_recv().is_ok() {}
                if let Ok(doc) = db.get(id).await {
                    return doc;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("live replication did not copy {id}"))
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

        // The existing document is copied, and so is one written while the
        // replication is running.
        let doc1 = wait_for_doc(&remote, "doc1", &mut rx).await;
        assert_eq!(doc1.data, serde_json::json!({"v": 1}));
        let r2 = local
            .put("doc2", serde_json::json!({"v": 2}))
            .await
            .unwrap();
        let doc2 = wait_for_doc(&remote, "doc2", &mut rx).await;
        assert_eq!(doc2.rev.unwrap().to_string(), r2.rev.unwrap());
        assert_eq!(doc2.data, serde_json::json!({"v": 2}));

        handle.cancel();
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
        assert!(
            matches!(result, Err(RouchError::DatabaseError(_))),
            "{result:?}"
        );
        assert!(matches!(db.get("doc1").await, Err(RouchError::NotFound(_))));
        assert_eq!(db.info().await.unwrap().update_seq, Seq::Num(0));
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
        let ids: Vec<_> = rest.results.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a5", "a6", "a7", "a8", "a9"]);
        assert_eq!(rest.last_seq, Seq::Num(20));

        // Invalid selectors are rejected.
        let invalid = db
            .changes(ChangesOptions {
                selector: Some(serde_json::json!({"type": {"$regex": "["}})),
                ..Default::default()
            })
            .await;
        assert!(
            matches!(invalid, Err(RouchError::BadRequest(_))),
            "{invalid:?}"
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

        let own = ids(users
            .all_docs(AllDocsOptions {
                key: Some("users:2".into()),
                ..AllDocsOptions::new()
            })
            .await
            .unwrap());
        assert_eq!(own, ["users:2"]);

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

    // -----------------------------------------------------------------
    // Delegation to the adapter (with and without plugins)
    // -----------------------------------------------------------------

    /// Adapter that records the calls it receives (and the `changes`
    /// options), delegating to a `MemoryAdapter`.
    ///
    /// Like CouchDB with a `validate_doc_update`, it answers documents listed
    /// in `reject` with that per-document error, and with `failures_only` it
    /// answers writes with only the failed documents (what CouchDB returns
    /// for `new_edits=false`).
    struct SpyAdapter {
        inner: MemoryAdapter,
        calls: std::sync::Mutex<Vec<&'static str>>,
        changes_opts: std::sync::Mutex<Vec<ChangesOptions>>,
        reject: HashMap<String, (Option<&'static str>, Option<&'static str>)>,
        failures_only: bool,
    }

    impl SpyAdapter {
        fn new() -> Self {
            Self {
                inner: MemoryAdapter::new("spy"),
                calls: Default::default(),
                changes_opts: Default::default(),
                reject: HashMap::new(),
                failures_only: false,
            }
        }

        fn rejecting(
            mut self,
            id: &str,
            error: Option<&'static str>,
            reason: Option<&'static str>,
        ) -> Self {
            self.reject.insert(id.to_string(), (error, reason));
            self
        }

        fn record(&self, call: &'static str) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Adapter for SpyAdapter {
        async fn info(&self) -> Result<DbInfo> {
            self.record("info");
            self.inner.info().await
        }
        async fn id(&self) -> Result<String> {
            self.record("id");
            self.inner.id().await
        }
        async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
            self.record("get");
            self.inner.get(id, opts).await
        }
        async fn bulk_docs(
            &self,
            docs: Vec<Document>,
            opts: BulkDocsOptions,
        ) -> Result<Vec<DocResult>> {
            self.record("bulk_docs");
            let mut results = Vec::new();
            for doc in docs {
                match self.reject.get(&doc.id) {
                    Some((error, reason)) => results.push(DocResult {
                        ok: false,
                        id: doc.id,
                        rev: None,
                        error: error.map(str::to_string),
                        reason: reason.map(str::to_string),
                    }),
                    None => results.extend(self.inner.bulk_docs(vec![doc], opts.clone()).await?),
                }
            }
            if self.failures_only {
                results.retain(|r| !r.ok);
            }
            Ok(results)
        }
        async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
            self.record("all_docs");
            self.inner.all_docs(opts).await
        }
        async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
            self.record("changes");
            self.changes_opts.lock().unwrap().push(opts.clone());
            self.inner.changes(opts).await
        }
        async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
            self.record("revs_diff");
            self.inner.revs_diff(revs).await
        }
        async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
            self.record("bulk_get");
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
            self.record("put_attachment");
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
            self.record("get_attachment");
            self.inner.get_attachment(doc_id, att_id, opts).await
        }
        async fn remove_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            rev: &str,
        ) -> Result<DocResult> {
            self.record("remove_attachment");
            self.inner.remove_attachment(doc_id, att_id, rev).await
        }
        async fn get_local(&self, id: &str) -> Result<serde_json::Value> {
            self.record("get_local");
            self.inner.get_local(id).await
        }
        async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
            self.record("put_local");
            self.inner.put_local(id, doc).await
        }
        async fn remove_local(&self, id: &str) -> Result<()> {
            self.record("remove_local");
            self.inner.remove_local(id).await
        }
        async fn compact(&self) -> Result<()> {
            self.record("compact");
            self.inner.compact().await
        }
        async fn destroy(&self) -> Result<()> {
            self.record("destroy");
            self.inner.destroy().await
        }
        async fn close(&self) -> Result<()> {
            self.record("close");
            self.inner.close().await
        }
        async fn purge(&self, req: HashMap<String, Vec<String>>) -> Result<PurgeResponse> {
            self.record("purge");
            self.inner.purge(req).await
        }
        async fn get_security(&self) -> Result<SecurityDocument> {
            self.record("get_security");
            self.inner.get_security().await
        }
        async fn put_security(&self, doc: SecurityDocument) -> Result<()> {
            self.record("put_security");
            self.inner.put_security(doc).await
        }
    }

    fn new_doc(id: &str, rev: Option<&str>, data: serde_json::Value) -> Document {
        Document {
            id: id.into(),
            rev: rev.map(|r| r.parse().unwrap()),
            deleted: false,
            data,
            attachments: HashMap::new(),
        }
    }

    fn security(admin: &str) -> SecurityDocument {
        SecurityDocument {
            admins: SecurityGroup {
                names: vec![admin.into()],
                roles: vec![],
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn database_delegates_to_the_given_adapter() {
        let spy = Arc::new(SpyAdapter::new());
        let db = Database::from_adapter(spy.clone());

        // Writes through the database land in the given adapter, and
        // `adapter()` is that adapter.
        db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        assert_eq!(
            spy.inner
                .get("doc1", GetOptions::default())
                .await
                .unwrap()
                .data["v"],
            1
        );
        assert_eq!(db.adapter().info().await.unwrap().db_name, "spy");
        assert_eq!(db.info().await.unwrap().doc_count, 1);

        db.put_security(security("bob")).await.unwrap();
        assert_eq!(
            spy.inner.get_security().await.unwrap().admins.names,
            ["bob"]
        );
        assert_eq!(db.get_security().await.unwrap().admins.names, ["bob"]);

        spy.calls.lock().unwrap().clear();
        db.compact().await.unwrap();
        db.close().await.unwrap();
        db.destroy().await.unwrap();
        assert_eq!(spy.calls(), ["compact", "close", "destroy"]);
        assert_eq!(spy.inner.info().await.unwrap().doc_count, 0);
    }

    struct NoopPlugin;

    #[async_trait::async_trait]
    impl Plugin for NoopPlugin {
        fn name(&self) -> &str {
            "noop"
        }
    }

    fn plugin_adapter(spy: &Arc<SpyAdapter>, plugins: Vec<Arc<dyn Plugin>>) -> PluginAdapter {
        PluginAdapter {
            inner: spy.clone(),
            plugins,
        }
    }

    #[tokio::test]
    async fn plugin_adapter_delegates_every_non_write_method() {
        // Replication writes into a PluginAdapter: everything but the writes
        // must reach the wrapped adapter unchanged.
        let spy = Arc::new(SpyAdapter::new());
        let pa = plugin_adapter(&spy, vec![Arc::new(NoopPlugin)]);
        let inner = &spy.inner;

        let r1 = inner
            .bulk_docs(
                vec![new_doc("d", None, serde_json::json!({"v": 1}))],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap()[0]
            .rev
            .clone()
            .unwrap();
        let r2 = inner
            .put_attachment("d", "a.txt", &r1, b"hello".to_vec(), "text/plain")
            .await
            .unwrap()
            .rev
            .unwrap();
        inner
            .put_local("cp", serde_json::json!({"seq": 1}))
            .await
            .unwrap();
        inner.put_security(security("alice")).await.unwrap();
        spy.calls.lock().unwrap().clear();

        assert_eq!(pa.id().await.unwrap(), "spy");
        assert_eq!(pa.info().await.unwrap().update_seq, Seq::Num(2));
        assert_eq!(
            pa.get("d", GetOptions::default())
                .await
                .unwrap()
                .rev
                .unwrap()
                .to_string(),
            r2
        );
        assert_eq!(
            pa.get_attachment("d", "a.txt", GetAttachmentOptions::default())
                .await
                .unwrap(),
            b"hello"
        );
        assert_eq!(pa.get_local("cp").await.unwrap()["seq"], 1);
        assert_eq!(pa.get_security().await.unwrap().admins.names, ["alice"]);
        assert_eq!(
            pa.all_docs(AllDocsOptions::new()).await.unwrap().rows.len(),
            1
        );
        assert_eq!(
            pa.changes(ChangesOptions::default())
                .await
                .unwrap()
                .last_seq,
            Seq::Num(2)
        );

        pa.put_local("cp", serde_json::json!({"seq": 2}))
            .await
            .unwrap();
        assert_eq!(inner.get_local("cp").await.unwrap()["seq"], 2);
        pa.remove_local("cp").await.unwrap();
        assert!(matches!(
            inner.get_local("cp").await,
            Err(RouchError::NotFound(_))
        ));

        pa.put_security(security("carol")).await.unwrap();
        assert_eq!(inner.get_security().await.unwrap().admins.names, ["carol"]);

        pa.compact().await.unwrap();
        let old = inner
            .get(
                "d",
                GetOptions {
                    rev: Some(r1.clone()),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(old, Err(RouchError::NotFound(_))), "{old:?}");

        pa.close().await.unwrap();
        pa.destroy().await.unwrap();
        assert_eq!(inner.info().await.unwrap().doc_count, 0);
        assert_eq!(
            spy.calls(),
            [
                "id",
                "info",
                "get",
                "get_attachment",
                "get_local",
                "get_security",
                "all_docs",
                "changes",
                "put_local",
                "remove_local",
                "put_security",
                "compact",
                "close",
                "destroy",
            ]
        );
    }

    /// Records every `after_write` batch.
    #[derive(Default)]
    struct AfterWriteLog(std::sync::Mutex<Vec<Vec<DocResult>>>);

    #[async_trait::async_trait]
    impl Plugin for AfterWriteLog {
        fn name(&self) -> &str {
            "after-write-log"
        }
        async fn after_write(&self, results: &[DocResult]) -> Result<()> {
            self.0.lock().unwrap().push(results.to_vec());
            Ok(())
        }
    }

    fn summary(results: &[DocResult]) -> Vec<(String, bool, Option<String>, Option<String>)> {
        let mut summary: Vec<_> = results
            .iter()
            .map(|r| (r.id.clone(), r.ok, r.rev.clone(), r.error.clone()))
            .collect();
        summary.sort();
        summary
    }

    #[tokio::test]
    async fn plugin_adapter_reports_every_replicated_doc_to_after_write() {
        // CouchDB answers new_edits=false writes with only the failures: the
        // documents it does not list were written under their own revision,
        // and after_write must see them as such.
        let mut spy = SpyAdapter::new().rejecting("bad", Some("forbidden"), Some("no"));
        spy.failures_only = true;
        let spy = Arc::new(spy);
        let log = Arc::new(AfterWriteLog::default());
        let pa = plugin_adapter(&spy, vec![log.clone()]);

        let results = pa
            .bulk_docs(
                vec![
                    new_doc("a", Some("1-aaa"), serde_json::json!({})),
                    new_doc("bad", Some("1-bbb"), serde_json::json!({})),
                    new_doc("c", Some("1-ccc"), serde_json::json!({})),
                ],
                BulkDocsOptions::replication(),
            )
            .await
            .unwrap();
        let expected = vec![
            ("a".to_string(), true, Some("1-aaa".to_string()), None),
            (
                "bad".to_string(),
                false,
                None,
                Some("forbidden".to_string()),
            ),
            ("c".to_string(), true, Some("1-ccc".to_string()), None),
        ];
        assert_eq!(summary(&results), expected);
        assert_eq!(log.0.lock().unwrap().len(), 1);
        assert_eq!(summary(&log.0.lock().unwrap()[0]), expected);

        // A new_edits=true write has no revision to report for an omitted
        // document, so nothing is made up.
        let results = pa
            .bulk_docs(
                vec![
                    new_doc("d", None, serde_json::json!({})),
                    new_doc("bad", None, serde_json::json!({})),
                ],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap();
        let expected = vec![(
            "bad".to_string(),
            false,
            None,
            Some("forbidden".to_string()),
        )];
        assert_eq!(summary(&results), expected);
        assert_eq!(summary(&log.0.lock().unwrap()[1]), expected);
    }

    /// Rejects documents by id: `u*` as unauthorized, `f*` as forbidden,
    /// `b*` as a bad request, `e*` with an unrelated error, and drops `x*`.
    struct GatePlugin;

    #[async_trait::async_trait]
    impl Plugin for GatePlugin {
        fn name(&self) -> &str {
            "gate"
        }
        async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
            for doc in docs.iter_mut() {
                match doc.id.chars().next() {
                    Some('u') => return Err(RouchError::Unauthorized),
                    Some('f') => return Err(RouchError::Forbidden("no f".into())),
                    Some('b') => return Err(RouchError::BadRequest("no b".into())),
                    Some('e') => return Err(RouchError::DatabaseError("broken".into())),
                    _ => doc.data["changed_by_plugin"] = serde_json::json!(true),
                }
            }
            docs.retain(|d| !d.id.starts_with('x'));
            Ok(())
        }
    }

    #[tokio::test]
    async fn plugin_adapter_denies_rejected_replicated_docs_one_by_one() {
        let spy = Arc::new(SpyAdapter::new());
        let pa = plugin_adapter(&spy, vec![Arc::new(GatePlugin)]);

        let results = pa
            .bulk_docs(
                vec![
                    new_doc("ok", Some("1-aaa"), serde_json::json!({"v": 1})),
                    new_doc("u1", Some("1-aaa"), serde_json::json!({})),
                    new_doc("f1", Some("1-aaa"), serde_json::json!({})),
                    new_doc("b1", Some("1-aaa"), serde_json::json!({})),
                    new_doc("x1", Some("1-aaa"), serde_json::json!({})),
                ],
                BulkDocsOptions::replication(),
            )
            .await
            .unwrap();
        assert_eq!(
            summary(&results),
            vec![
                ("b1".to_string(), false, None, Some("forbidden".to_string())),
                ("f1".to_string(), false, None, Some("forbidden".to_string())),
                ("ok".to_string(), true, Some("1-aaa".to_string()), None),
                (
                    "u1".to_string(),
                    false,
                    None,
                    Some("unauthorized".to_string())
                ),
                ("x1".to_string(), false, None, Some("forbidden".to_string())),
            ]
        );
        let dropped = results.iter().find(|r| r.id == "x1").unwrap();
        assert!(
            dropped
                .reason
                .as_deref()
                .unwrap()
                .contains("dropped by plugin gate"),
            "{dropped:?}"
        );

        // Only the accepted doc is stored, with the source's body.
        let ids: Vec<String> = spy
            .inner
            .all_docs(AllDocsOptions::new())
            .await
            .unwrap()
            .rows
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["ok"]);
        let stored = spy.inner.get("ok", GetOptions::default()).await.unwrap();
        assert_eq!(stored.data, serde_json::json!({"v": 1}));

        // Any other plugin error fails the whole write.
        let failed = pa
            .bulk_docs(
                vec![
                    new_doc("ok2", Some("1-aaa"), serde_json::json!({})),
                    new_doc("e1", Some("1-aaa"), serde_json::json!({})),
                ],
                BulkDocsOptions::replication(),
            )
            .await;
        assert!(
            matches!(failed, Err(RouchError::DatabaseError(_))),
            "{failed:?}"
        );
        assert!(matches!(
            spy.inner.get("ok2", GetOptions::default()).await,
            Err(RouchError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn single_doc_write_errors_keep_their_kind() {
        // A per-document failure reported by the adapter becomes the
        // matching error (what CouchDB's validate_doc_update produces).
        let spy = SpyAdapter::new()
            .rejecting("f", Some("forbidden"), Some("no f"))
            .rejecting("u", Some("unauthorized"), Some("no u"))
            .rejecting("n", Some("not_found"), Some("missing"))
            .rejecting("c", Some("conflict"), Some("Document update conflict."))
            .rejecting("o", Some("some_other_error"), Some("odd"))
            .rejecting("r", Some("some_other_error"), None)
            .rejecting("none", None, None);
        let db = Database::from_adapter(Arc::new(spy));
        let put = |id: &'static str| {
            let db = &db;
            async move { db.put(id, serde_json::json!({})).await }
        };
        let forbidden = put("f").await;
        assert!(
            matches!(&forbidden, Err(RouchError::Forbidden(reason)) if reason == "no f"),
            "{forbidden:?}"
        );
        let unauthorized = put("u").await;
        assert!(
            matches!(unauthorized, Err(RouchError::Unauthorized)),
            "{unauthorized:?}"
        );
        let not_found = put("n").await;
        assert!(
            matches!(&not_found, Err(RouchError::NotFound(id)) if id == "n"),
            "{not_found:?}"
        );
        let conflict = put("c").await;
        assert!(
            matches!(conflict, Err(RouchError::Conflict)),
            "{conflict:?}"
        );
        let other = put("o").await;
        assert!(
            matches!(&other, Err(RouchError::BadRequest(reason)) if reason == "odd"),
            "{other:?}"
        );
        let no_reason = put("r").await;
        assert!(
            matches!(&no_reason, Err(RouchError::BadRequest(reason)) if reason == "some_other_error"),
            "{no_reason:?}"
        );
        let nothing = put("none").await;
        assert!(
            matches!(&nothing, Err(RouchError::BadRequest(reason)) if reason == "document write failed"),
            "{nothing:?}"
        );
        assert_eq!(db.info().await.unwrap().update_seq, Seq::Num(0));
    }

    // -----------------------------------------------------------------
    // Changes filtered by a selector
    // -----------------------------------------------------------------

    /// A spied database with `n` documents `d0000`, `d0001`, ... (`{"n": i}`).
    async fn numbered_docs(n: usize) -> (Arc<SpyAdapter>, Database) {
        let spy = Arc::new(SpyAdapter::new());
        let docs = (0..n)
            .map(|i| new_doc(&format!("d{i:04}"), None, serde_json::json!({"n": i})))
            .collect();
        spy.inner
            .bulk_docs(docs, BulkDocsOptions::new())
            .await
            .unwrap();
        (spy.clone(), Database::from_adapter(spy))
    }

    #[tokio::test]
    async fn indexed_find_reads_only_the_changes_since_the_index() {
        // The index is brought up to date from where it left off, not by
        // re-reading the whole changes feed on every query.
        let (spy, db) = numbered_docs(3).await;
        db.create_index(IndexDefinition {
            name: String::new(),
            fields: vec![SortField::Simple("n".into())],
            ddoc: None,
        })
        .await
        .unwrap();
        let find = || {
            let db = &db;
            async move {
                let docs = db
                    .find(FindOptions {
                        selector: serde_json::json!({"n": {"$gte": 1}}),
                        ..Default::default()
                    })
                    .await
                    .unwrap()
                    .docs;
                docs.iter()
                    .map(|d| d["_id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            }
        };
        let sinces = || {
            std::mem::take(&mut *spy.changes_opts.lock().unwrap())
                .into_iter()
                .map(|o| o.since)
                .collect::<Vec<_>>()
        };
        sinces();

        assert_eq!(find().await, ["d0001", "d0002"]);
        assert_eq!(sinces(), [Seq::Num(3)]);
        db.put("d0003", serde_json::json!({"n": 3})).await.unwrap();
        assert_eq!(find().await, ["d0001", "d0002", "d0003"]);
        assert_eq!(sinces(), [Seq::Num(3)]);
        assert_eq!(find().await, ["d0001", "d0002", "d0003"]);
        assert_eq!(sinces(), [Seq::Num(4)]);
    }

    #[tokio::test]
    async fn selector_changes_read_the_feed_in_batches() {
        // The matching changes are past the first batches: the feed must be
        // read on until `limit` matches are found.
        let total = 2 * SELECTOR_CHANGES_BATCH as usize + 200;
        let (spy, db) = numbered_docs(total).await;
        let first = total - 150;
        let changes = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            db.changes(ChangesOptions {
                selector: Some(serde_json::json!({"n": {"$gte": first}})),
                limit: Some(3),
                ..Default::default()
            }),
        )
        .await
        .expect("selector changes did not finish")
        .unwrap();
        let ids: Vec<&str> = changes.results.iter().map(|c| c.id.as_str()).collect();
        let expected: Vec<String> = (first..first + 3).map(|i| format!("d{i:04}")).collect();
        assert_eq!(ids, expected);
        assert_eq!(changes.last_seq, Seq::Num(first as u64 + 3));
        assert!(changes.results.iter().all(|c| c.doc.is_none()));

        // The adapter is asked for unfiltered batches with the documents;
        // the selector is applied here.
        let opts = spy.changes_opts.lock().unwrap().clone();
        assert_eq!(opts.len(), 3, "{opts:?}");
        let sinces: Vec<Seq> = opts.iter().map(|o| o.since.clone()).collect();
        let batch = SELECTOR_CHANGES_BATCH;
        assert_eq!(sinces, [Seq::Num(0), Seq::Num(batch), Seq::Num(2 * batch)]);
        for o in &opts {
            assert!(o.selector.is_none(), "{o:?}");
            assert!(o.include_docs, "{o:?}");
            assert_eq!(o.limit, Some(batch), "{o:?}");
        }
    }

    #[tokio::test]
    async fn descending_selector_changes_read_the_feed_whole() {
        // A descending feed cannot be resumed from a sequence, so it is read
        // in one go and filtered from the newest change down.
        let total = SELECTOR_CHANGES_BATCH as usize + 100;
        let (spy, db) = numbered_docs(total).await;
        let changes = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            db.changes(ChangesOptions {
                selector: Some(serde_json::json!({"n": {"$lt": 3}})),
                descending: true,
                limit: Some(2),
                include_docs: true,
                ..Default::default()
            }),
        )
        .await
        .expect("descending selector changes did not finish")
        .unwrap();
        let ids: Vec<&str> = changes.results.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["d0002", "d0001"]);
        assert_eq!(changes.last_seq, Seq::Num(2));
        assert_eq!(changes.results[0].doc.as_ref().unwrap()["n"], 2);

        let opts = spy.changes_opts.lock().unwrap().clone();
        assert_eq!(opts.len(), 1, "{opts:?}");
        assert!(opts[0].descending);
        assert_eq!(opts[0].limit, None);
        assert!(opts[0].selector.is_none());
    }
}
