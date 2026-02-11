use std::collections::HashMap;

use async_trait::async_trait;

use crate::document::*;
use crate::error::Result;

/// The trait all storage adapters must implement.
///
/// This mirrors PouchDB's internal adapter interface (underscore-prefixed
/// methods in JavaScript). Each method corresponds to a CouchDB API endpoint.
///
/// Adapters are responsible for:
/// - Storing and retrieving documents with full revision tree support
/// - Tracking sequence numbers for the changes feed
/// - Managing local (non-replicated) documents for checkpoints
/// - Attachment storage and retrieval
#[async_trait]
pub trait Adapter: Send + Sync {
    /// Get database information: name, document count, update sequence.
    async fn info(&self) -> Result<DbInfo>;

    /// Retrieve a single document by ID.
    ///
    /// Supports fetching specific revisions, open revisions (all leaves),
    /// and including conflict information.
    async fn get(&self, id: &str, opts: GetOptions) -> Result<crate::document::Document>;

    /// Write multiple documents atomically.
    ///
    /// When `opts.new_edits` is `true` (default), the adapter generates new
    /// revision IDs and checks for conflicts.
    ///
    /// When `opts.new_edits` is `false` (replication mode), the adapter
    /// accepts revision IDs as-is and merges them into the existing revision
    /// tree without conflict checks.
    async fn bulk_docs(
        &self,
        docs: Vec<crate::document::Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>>;

    /// Query all documents, optionally filtered by key range.
    async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse>;

    /// Get changes since a given sequence number.
    async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse>;

    /// Compare sets of document revisions to find which ones the adapter
    /// is missing. Used during replication to avoid transferring data the
    /// target already has.
    async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse>;

    /// Fetch multiple documents by ID and revision in a single request.
    /// Used during replication to efficiently retrieve missing documents.
    async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse>;

    /// Store an attachment on a document.
    async fn put_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<DocResult>;

    /// Retrieve raw attachment data.
    async fn get_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>>;

    /// Remove an attachment from a document.
    ///
    /// Creates a new revision of the document with the attachment removed.
    async fn remove_attachment(&self, doc_id: &str, att_id: &str, rev: &str) -> Result<DocResult>;

    /// Retrieve a local document (not replicated, used for checkpoints).
    async fn get_local(&self, id: &str) -> Result<serde_json::Value>;

    /// Write a local document (not replicated, used for checkpoints).
    async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()>;

    /// Remove a local document.
    async fn remove_local(&self, id: &str) -> Result<()>;

    /// Compact the database: remove old revisions, clean up unreferenced
    /// attachment data.
    async fn compact(&self) -> Result<()>;

    /// Destroy the database and all its data.
    async fn destroy(&self) -> Result<()>;

    /// Close the database, releasing any held resources.
    /// Default implementation is a no-op.
    async fn close(&self) -> Result<()> {
        Ok(())
    }

    /// Purge (permanently remove) document revisions.
    async fn purge(
        &self,
        _req: HashMap<String, Vec<String>>,
    ) -> Result<crate::document::PurgeResponse> {
        Err(crate::error::RouchError::BadRequest(
            "purge not supported".into(),
        ))
    }

    /// Get the security document for this database.
    async fn get_security(&self) -> Result<crate::document::SecurityDocument> {
        Ok(crate::document::SecurityDocument::default())
    }

    /// Set the security document for this database.
    async fn put_security(&self, _doc: crate::document::SecurityDocument) -> Result<()> {
        Ok(())
    }
}
