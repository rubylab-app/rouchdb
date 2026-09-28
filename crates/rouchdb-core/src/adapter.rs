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

    /// A stable identifier for this database, used to derive replication
    /// ids so that replications with different peers never share a
    /// checkpoint. Defaults to the database name; remote adapters should
    /// include the server identity.
    async fn id(&self) -> Result<String> {
        Ok(self.info().await?.db_name)
    }

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
    ///
    /// The default returns an empty document (no admins or members), which
    /// is what an adapter without security support effectively enforces.
    async fn get_security(&self) -> Result<crate::document::SecurityDocument> {
        Ok(crate::document::SecurityDocument::default())
    }

    /// Set the security document for this database.
    ///
    /// The default returns an error: an adapter that cannot store the
    /// document must not report success and silently drop it.
    async fn put_security(&self, _doc: crate::document::SecurityDocument) -> Result<()> {
        Err(crate::error::RouchError::BadRequest(
            "put_security not supported".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RouchError;

    /// An adapter that only implements the required methods.
    struct Minimal;

    #[async_trait]
    impl Adapter for Minimal {
        async fn info(&self) -> Result<DbInfo> {
            unimplemented!()
        }
        async fn get(&self, _: &str, _: GetOptions) -> Result<Document> {
            unimplemented!()
        }
        async fn bulk_docs(&self, _: Vec<Document>, _: BulkDocsOptions) -> Result<Vec<DocResult>> {
            unimplemented!()
        }
        async fn all_docs(&self, _: AllDocsOptions) -> Result<AllDocsResponse> {
            unimplemented!()
        }
        async fn changes(&self, _: ChangesOptions) -> Result<ChangesResponse> {
            unimplemented!()
        }
        async fn revs_diff(&self, _: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
            unimplemented!()
        }
        async fn bulk_get(&self, _: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
            unimplemented!()
        }
        async fn put_attachment(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Vec<u8>,
            _: &str,
        ) -> Result<DocResult> {
            unimplemented!()
        }
        async fn get_attachment(
            &self,
            _: &str,
            _: &str,
            _: GetAttachmentOptions,
        ) -> Result<Vec<u8>> {
            unimplemented!()
        }
        async fn remove_attachment(&self, _: &str, _: &str, _: &str) -> Result<DocResult> {
            unimplemented!()
        }
        async fn get_local(&self, _: &str) -> Result<serde_json::Value> {
            unimplemented!()
        }
        async fn put_local(&self, _: &str, _: serde_json::Value) -> Result<()> {
            unimplemented!()
        }
        async fn remove_local(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn compact(&self) -> Result<()> {
            unimplemented!()
        }
        async fn destroy(&self) -> Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn default_put_security_is_not_a_silent_success() {
        // An adapter that cannot store a security document must say so
        // instead of reporting success and dropping it.
        let res = Minimal.put_security(SecurityDocument::default()).await;
        assert!(matches!(res, Err(RouchError::BadRequest(_))), "{:?}", res);
        assert!(Minimal.purge(HashMap::new()).await.is_err());
    }
}
