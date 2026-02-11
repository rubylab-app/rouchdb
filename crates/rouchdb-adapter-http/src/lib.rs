/// HTTP adapter for RouchDB.
///
/// Communicates with a remote CouchDB-compatible server via HTTP,
/// implementing the Adapter trait by mapping each method to the
/// corresponding CouchDB REST API endpoint.
pub mod auth;

use std::collections::HashMap;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::{Result, RouchError};

// ---------------------------------------------------------------------------
// CouchDB JSON response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CouchDbInfo {
    db_name: String,
    doc_count: u64,
    update_seq: serde_json::Value, // Can be integer or string depending on CouchDB version
}

#[derive(Debug, Deserialize)]
struct CouchDbPutResponse {
    ok: Option<bool>,
    id: String,
    rev: String,
}

#[derive(Debug, Deserialize)]
struct CouchDbError {
    #[allow(dead_code)]
    error: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct CouchDbBulkDocsRequest {
    docs: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_edits: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CouchDbBulkDocsResult {
    ok: Option<bool>,
    id: Option<String>,
    rev: Option<String>,
    error: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct CouchDbBulkGetRequest {
    docs: Vec<CouchDbBulkGetDoc>,
}

#[derive(Debug, Serialize)]
struct CouchDbBulkGetDoc {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    rev: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CouchDbBulkGetResponse {
    results: Vec<CouchDbBulkGetResult>,
}

#[derive(Debug, Deserialize)]
struct CouchDbBulkGetResult {
    id: String,
    docs: Vec<CouchDbBulkGetDocResult>,
}

#[derive(Debug, Deserialize)]
struct CouchDbBulkGetDocResult {
    ok: Option<serde_json::Value>,
    error: Option<CouchDbBulkGetErrorResult>,
}

#[derive(Debug, Deserialize)]
struct CouchDbBulkGetErrorResult {
    id: String,
    rev: String,
    error: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct CouchDbChangesResponse {
    results: Vec<CouchDbChangeResult>,
    last_seq: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CouchDbChangeResult {
    seq: serde_json::Value,
    id: String,
    changes: Vec<CouchDbChangeRev>,
    #[serde(default)]
    deleted: bool,
    doc: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CouchDbChangeRev {
    rev: String,
}

#[derive(Debug, Deserialize)]
struct CouchDbAllDocsResponse {
    total_rows: u64,
    offset: u64,
    rows: Vec<CouchDbAllDocsRow>,
}

#[derive(Debug, Deserialize)]
struct CouchDbAllDocsRow {
    id: String,
    key: String,
    value: CouchDbAllDocsRowValue,
    doc: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CouchDbAllDocsRowValue {
    rev: String,
    #[serde(default)]
    deleted: Option<bool>,
}

// ---------------------------------------------------------------------------
// HttpAdapter
// ---------------------------------------------------------------------------

/// HTTP adapter that talks to a remote CouchDB instance.
pub struct HttpAdapter {
    client: Client,
    base_url: String,
}

impl HttpAdapter {
    /// Create a new HTTP adapter pointing at a CouchDB database URL.
    ///
    /// The URL should include the database name, e.g.
    /// `http://localhost:5984/mydb` or `http://admin:password@localhost:5984/mydb`
    pub fn new(url: &str) -> Self {
        let base_url = url.trim_end_matches('/').to_string();
        Self {
            client: Client::new(),
            base_url,
        }
    }

    /// Create a new HTTP adapter with a custom reqwest client.
    pub fn with_client(url: &str, client: Client) -> Self {
        let base_url = url.trim_end_matches('/').to_string();
        Self { client, base_url }
    }

    /// Create a new HTTP adapter using an authenticated client.
    ///
    /// The `AuthClient` must have been logged in already; its internal
    /// reqwest client (with cookie store) will be shared with this adapter.
    pub fn with_auth_client(url: &str, auth: &auth::AuthClient) -> Self {
        Self::with_client(url, auth.client().clone())
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    async fn check_error(&self, response: reqwest::Response) -> Result<reqwest::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }

        match status.as_u16() {
            401 => Err(RouchError::Unauthorized),
            403 => {
                let body: CouchDbError = response.json().await.unwrap_or(CouchDbError {
                    error: "forbidden".into(),
                    reason: "access denied".into(),
                });
                Err(RouchError::Forbidden(body.reason))
            }
            404 => {
                let body: CouchDbError = response.json().await.unwrap_or(CouchDbError {
                    error: "not_found".into(),
                    reason: "missing".into(),
                });
                Err(RouchError::NotFound(body.reason))
            }
            409 => Err(RouchError::Conflict),
            _ => {
                let body = response.text().await.unwrap_or_default();
                Err(RouchError::DatabaseError(format!(
                    "HTTP {}: {}",
                    status, body
                )))
            }
        }
    }
}

/// Parse a CouchDB sequence value (can be integer or string).
fn parse_seq(value: &serde_json::Value) -> Seq {
    match value {
        serde_json::Value::Number(n) => Seq::Num(n.as_u64().unwrap_or(0)),
        serde_json::Value::String(s) => {
            if let Ok(n) = s.parse::<u64>() {
                Seq::Num(n)
            } else {
                Seq::Str(s.clone())
            }
        }
        _ => Seq::Num(0),
    }
}

#[async_trait]
impl Adapter for HttpAdapter {
    async fn info(&self) -> Result<DbInfo> {
        let resp = self
            .client
            .get(&self.base_url)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let info: CouchDbInfo = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(DbInfo {
            db_name: info.db_name,
            doc_count: info.doc_count,
            update_seq: parse_seq(&info.update_seq),
        })
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        let mut url = self.url(&urlencoded(id));
        let mut params = Vec::new();

        if let Some(ref rev) = opts.rev {
            params.push(format!("rev={}", rev));
        }
        if opts.conflicts {
            params.push("conflicts=true".into());
        }
        if opts.revs {
            params.push("revs=true".into());
        }
        if opts.revs_info {
            params.push("revs_info=true".into());
        }
        if opts.latest {
            params.push("latest=true".into());
        }
        if opts.attachments {
            params.push("attachments=true".into());
        }
        if let Some(ref open_revs) = opts.open_revs {
            match open_revs {
                OpenRevs::All => params.push("open_revs=all".into()),
                OpenRevs::Specific(revs) => {
                    let json = serde_json::to_string(revs).unwrap_or_default();
                    params.push(format!("open_revs={}", json));
                }
            }
        }

        if !params.is_empty() {
            url = format!("{}?{}", url, params.join("&"));
        }

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Document::from_json(json)
    }

    async fn bulk_docs(
        &self,
        docs: Vec<Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>> {
        let json_docs: Vec<serde_json::Value> = docs.iter().map(|d| d.to_json()).collect();

        let request = CouchDbBulkDocsRequest {
            docs: json_docs,
            new_edits: if opts.new_edits { None } else { Some(false) },
        };

        let resp = self
            .client
            .post(self.url("_bulk_docs"))
            .json(&request)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;

        let results: Vec<CouchDbBulkDocsResult> = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(results
            .into_iter()
            .map(|r| DocResult {
                ok: r.ok.unwrap_or(r.error.is_none()),
                id: r.id.unwrap_or_default(),
                rev: r.rev,
                error: r.error,
                reason: r.reason,
            })
            .collect())
    }

    async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
        let mut params = Vec::new();
        if opts.include_docs {
            params.push("include_docs=true".into());
        }
        if opts.descending {
            params.push("descending=true".into());
        }
        if let Some(ref start) = opts.start_key {
            params.push(format!("startkey=\"{}\"", start));
        }
        if let Some(ref end) = opts.end_key {
            params.push(format!("endkey=\"{}\"", end));
        }
        if let Some(limit) = opts.limit {
            params.push(format!("limit={}", limit));
        }
        if opts.skip > 0 {
            params.push(format!("skip={}", opts.skip));
        }
        if opts.conflicts {
            params.push("conflicts=true".into());
        }
        if opts.update_seq {
            params.push("update_seq=true".into());
        }

        let mut url = self.url("_all_docs");
        if !params.is_empty() {
            url = format!("{}?{}", url, params.join("&"));
        }

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let result: CouchDbAllDocsResponse = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(AllDocsResponse {
            total_rows: result.total_rows,
            offset: result.offset,
            rows: result
                .rows
                .into_iter()
                .map(|r| AllDocsRow {
                    id: r.id,
                    key: r.key,
                    value: AllDocsRowValue {
                        rev: r.value.rev,
                        deleted: r.value.deleted,
                    },
                    doc: r.doc,
                })
                .collect(),
            update_seq: None, // TODO: parse from CouchDB response when update_seq=true
        })
    }

    async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
        let mut params = vec![format!("since={}", opts.since.to_query_string())];
        if opts.include_docs {
            params.push("include_docs=true".into());
        }
        if opts.descending {
            params.push("descending=true".into());
        }
        if let Some(limit) = opts.limit {
            params.push(format!("limit={}", limit));
        }

        if opts.conflicts {
            params.push("conflicts=true".into());
        }
        if opts.style == ChangesStyle::AllDocs {
            params.push("style=all_docs".into());
        }

        // Determine which filter to use — doc_ids and selector are mutually exclusive
        let use_post = opts.doc_ids.is_some() || opts.selector.is_some();
        if opts.doc_ids.is_some() {
            params.push("filter=_doc_ids".into());
        } else if opts.selector.is_some() {
            params.push("filter=_selector".into());
        }

        let url = format!("{}?{}", self.url("_changes"), params.join("&"));

        let resp = if use_post {
            let body = if let Some(doc_ids) = opts.doc_ids {
                serde_json::json!({ "doc_ids": doc_ids })
            } else if let Some(selector) = opts.selector {
                serde_json::json!({ "selector": selector })
            } else {
                serde_json::json!({})
            };
            self.client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| RouchError::DatabaseError(e.to_string()))?
        } else {
            self.client
                .get(&url)
                .send()
                .await
                .map_err(|e| RouchError::DatabaseError(e.to_string()))?
        };

        let resp = self.check_error(resp).await?;
        let result: CouchDbChangesResponse = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(ChangesResponse {
            last_seq: parse_seq(&result.last_seq),
            results: result
                .results
                .into_iter()
                .map(|r| ChangeEvent {
                    seq: parse_seq(&r.seq),
                    id: r.id,
                    changes: r
                        .changes
                        .into_iter()
                        .map(|c| ChangeRev { rev: c.rev })
                        .collect(),
                    deleted: r.deleted,
                    doc: r.doc,
                    conflicts: None, // CouchDB includes these inline in the doc
                })
                .collect(),
        })
    }

    async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
        let resp = self
            .client
            .post(self.url("_revs_diff"))
            .json(&revs)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;

        let results: HashMap<String, RevsDiffResult> = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(RevsDiffResponse { results })
    }

    async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
        let request = CouchDbBulkGetRequest {
            docs: docs
                .into_iter()
                .map(|d| CouchDbBulkGetDoc {
                    id: d.id,
                    rev: d.rev,
                })
                .collect(),
        };

        let resp = self
            .client
            .post(self.url("_bulk_get?revs=true"))
            .json(&request)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;

        let result: CouchDbBulkGetResponse = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(BulkGetResponse {
            results: result
                .results
                .into_iter()
                .map(|r| BulkGetResult {
                    id: r.id,
                    docs: r
                        .docs
                        .into_iter()
                        .map(|d| BulkGetDoc {
                            ok: d.ok,
                            error: d.error.map(|e| BulkGetError {
                                id: e.id,
                                rev: e.rev,
                                error: e.error,
                                reason: e.reason,
                            }),
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    async fn put_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<DocResult> {
        let url = format!(
            "{}/{}?rev={}",
            self.url(&urlencoded(doc_id)),
            urlencoded(att_id),
            rev
        );

        let resp = self
            .client
            .put(&url)
            .header("Content-Type", content_type)
            .body(data)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let result: CouchDbPutResponse = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(DocResult {
            ok: result.ok.unwrap_or(true),
            id: result.id,
            rev: Some(result.rev),
            error: None,
            reason: None,
        })
    }

    async fn get_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>> {
        let mut url = format!("{}/{}", self.url(&urlencoded(doc_id)), urlencoded(att_id));
        if let Some(ref rev) = opts.rev {
            url = format!("{}?rev={}", url, rev);
        }

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(bytes.to_vec())
    }

    async fn remove_attachment(&self, doc_id: &str, att_id: &str, rev: &str) -> Result<DocResult> {
        let url = format!(
            "{}/{}?rev={}",
            self.url(&urlencoded(doc_id)),
            urlencoded(att_id),
            rev
        );

        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let result: CouchDbPutResponse = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(DocResult {
            ok: result.ok.unwrap_or(true),
            id: result.id,
            rev: Some(result.rev),
            error: None,
            reason: None,
        })
    }

    async fn get_local(&self, id: &str) -> Result<serde_json::Value> {
        let url = self.url(&format!("_local/{}", urlencoded(id)));
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        Ok(json)
    }

    async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
        let url = self.url(&format!("_local/{}", urlencoded(id)));
        let resp = self
            .client
            .put(&url)
            .json(&doc)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        self.check_error(resp).await?;
        Ok(())
    }

    async fn remove_local(&self, id: &str) -> Result<()> {
        // Need to get the current rev first
        let doc = self.get_local(id).await?;
        let rev = doc["_rev"].as_str().unwrap_or("");
        let url = format!(
            "{}?rev={}",
            self.url(&format!("_local/{}", urlencoded(id))),
            rev
        );
        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        self.check_error(resp).await?;
        Ok(())
    }

    async fn compact(&self) -> Result<()> {
        let resp = self
            .client
            .post(self.url("_compact"))
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        self.check_error(resp).await?;
        Ok(())
    }

    async fn destroy(&self) -> Result<()> {
        let resp = self
            .client
            .delete(&self.base_url)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        self.check_error(resp).await?;
        Ok(())
    }

    async fn purge(&self, req: HashMap<String, Vec<String>>) -> Result<PurgeResponse> {
        let resp = self
            .client
            .post(self.url("_purge"))
            .json(&req)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let result: PurgeResponse = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        Ok(result)
    }

    async fn get_security(&self) -> Result<SecurityDocument> {
        let resp = self
            .client
            .get(self.url("_security"))
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let doc: SecurityDocument = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        Ok(doc)
    }

    async fn put_security(&self, doc: SecurityDocument) -> Result<()> {
        let resp = self
            .client
            .put(self.url("_security"))
            .json(&doc)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        self.check_error(resp).await?;
        Ok(())
    }
}

/// Percent-encode a CouchDB document or attachment ID for safe URL use.
///
/// Encodes all characters except unreserved ones (alphanumeric, `-`, `_`, `.`, `~`).
/// This ensures IDs containing `@`, `&`, `=`, `/`, `+`, spaces, etc. are handled correctly.
fn urlencoded(s: &str) -> String {
    /// Characters that do NOT need encoding in a path segment.
    /// RFC 3986 unreserved: ALPHA / DIGIT / "-" / "." / "_" / "~"
    const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::percent_encode(s.as_bytes(), UNRESERVED).to_string()
}
