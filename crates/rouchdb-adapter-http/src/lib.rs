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
    #[serde(default)]
    doc_del_count: u64,
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
    // CouchDB sends `"offset": null` when `keys` are posted.
    offset: Option<u64>,
    rows: Vec<CouchDbAllDocsRow>,
    /// Present when `update_seq=true` was requested.
    #[serde(default)]
    update_seq: Option<serde_json::Value>,
}

/// A row of `_all_docs`. When `keys` are posted, keys that do not exist come
/// back as `{"key": "x", "error": "not_found"}` with no `id` or `value`.
#[derive(Debug, Deserialize)]
struct CouchDbAllDocsRow {
    id: Option<String>,
    key: String,
    value: Option<CouchDbAllDocsRowValue>,
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

/// Options for [`HttpAdapter::with_options`].
#[derive(Debug, Clone, Default)]
pub struct HttpAdapterOptions {
    /// Do not create the remote database on first use (PouchDB's
    /// `skip_setup`): operations on a missing database fail with NotFound.
    pub skip_setup: bool,
}

/// HTTP adapter that talks to a remote CouchDB instance.
///
/// Like PouchDB, the remote database is created on first use if it does not
/// exist yet, unless [`HttpAdapterOptions::skip_setup`] is set.
pub struct HttpAdapter {
    client: Client,
    base_url: String,
    skip_setup: bool,
    /// Set once the remote database is known to exist.
    setup: tokio::sync::OnceCell<()>,
}

impl HttpAdapter {
    /// Create a new HTTP adapter pointing at a CouchDB database URL.
    ///
    /// The URL should include the database name, e.g.
    /// `http://localhost:5984/mydb` or `http://admin:password@localhost:5984/mydb`
    pub fn new(url: &str) -> Self {
        Self::with_options(url, HttpAdapterOptions::default())
    }

    /// Create a new HTTP adapter with explicit options.
    pub fn with_options(url: &str, opts: HttpAdapterOptions) -> Self {
        let mut adapter = Self::with_client(url, Client::new());
        adapter.skip_setup = opts.skip_setup;
        adapter
    }

    /// Create a new HTTP adapter with a custom reqwest client.
    pub fn with_client(url: &str, client: Client) -> Self {
        let base_url = url.trim_end_matches('/').to_string();
        Self {
            client,
            base_url,
            skip_setup: false,
            setup: tokio::sync::OnceCell::new(),
        }
    }

    /// Create a new HTTP adapter using an authenticated client.
    ///
    /// The `AuthClient` must have been logged in already; its internal
    /// reqwest client (with cookie store) will be shared with this adapter.
    pub fn with_auth_client(url: &str, auth: &auth::AuthClient) -> Self {
        Self::with_client(url, auth.client().clone())
    }

    /// Make sure the remote database exists, creating it when missing, as
    /// PouchDB does. Runs once per adapter; a failure is retried next call.
    async fn ensure_setup(&self) -> Result<()> {
        if self.skip_setup {
            return Ok(());
        }
        self.setup
            .get_or_try_init(|| async {
                let resp = self
                    .client
                    .get(&self.base_url)
                    .send()
                    .await
                    .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
                if resp.status() != reqwest::StatusCode::NOT_FOUND {
                    self.check_error(resp).await?;
                    return Ok(());
                }
                let resp = self
                    .client
                    .put(&self.base_url)
                    .send()
                    .await
                    .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
                // 412: created concurrently by someone else.
                if resp.status() != reqwest::StatusCode::PRECONDITION_FAILED {
                    self.check_error(resp).await?;
                }
                Ok(())
            })
            .await
            .map(|_| ())
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
        self.ensure_setup().await?;
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
            doc_del_count: info.doc_del_count,
            update_seq: parse_seq(&info.update_seq),
        })
    }

    async fn id(&self) -> Result<String> {
        // Like PouchDB: the server's uuid plus the database name, so every
        // URL of the same database maps to one replication id (and same-named
        // databases on different servers do not); otherwise the URL without
        // credentials.
        let (server, db) = self
            .base_url
            .rsplit_once('/')
            .unwrap_or((self.base_url.as_str(), ""));
        let uuid = async {
            let resp = self.client.get(server).send().await.ok()?;
            let root: serde_json::Value = resp.error_for_status().ok()?.json().await.ok()?;
            root.get("uuid")?.as_str().map(String::from)
        }
        .await;
        Ok(match uuid {
            Some(uuid) => format!("{}{}", uuid, db),
            None => url_without_credentials(&self.base_url),
        })
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        self.ensure_setup().await?;
        let mut url = self.url(&encode_doc_id(id));
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
                    params.push(format!("open_revs={}", urlencoded(&json)));
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
        self.ensure_setup().await?;
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
        self.ensure_setup().await?;
        let mut params = Vec::new();
        if opts.include_docs {
            params.push("include_docs=true".into());
        }
        if opts.descending {
            params.push("descending=true".into());
        }
        if let Some(ref start) = opts.start_key {
            params.push(format!("startkey={}", encode_query_key(start)));
        }
        if let Some(ref end) = opts.end_key {
            params.push(format!("endkey={}", encode_query_key(end)));
        }
        if let Some(ref k) = opts.key {
            params.push(format!("key={}", encode_query_key(k)));
        }
        if !opts.inclusive_end {
            params.push("inclusive_end=false".into());
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

        // Multiple keys are sent via a POST body (CouchDB `_all_docs` keys
        // form); everything else is a GET.
        let resp = if let Some(ref keys) = opts.keys {
            self.client
                .post(&url)
                .json(&serde_json::json!({ "keys": keys }))
                .send()
                .await
        } else {
            self.client.get(&url).send().await
        }
        .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let result: CouchDbAllDocsResponse = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        Ok(AllDocsResponse {
            total_rows: result.total_rows,
            offset: result.offset.unwrap_or(0),
            // Skip `not_found` rows for missing keys, like the local adapters.
            rows: result
                .rows
                .into_iter()
                .filter_map(|r| {
                    let (id, value) = (r.id?, r.value?);
                    Some(AllDocsRow {
                        id,
                        key: r.key,
                        value: AllDocsRowValue {
                            rev: value.rev,
                            deleted: value.deleted,
                        },
                        doc: r.doc,
                    })
                })
                .collect(),
            update_seq: result.update_seq.as_ref().map(parse_seq),
        })
    }

    async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
        self.ensure_setup().await?;
        let mut params = vec![format!("since={}", opts.since.to_query_string())];
        // CouchDB only reports conflicts inside included docs (`_conflicts`),
        // so fetch the docs for them and drop them afterwards if unwanted.
        if opts.include_docs || opts.conflicts {
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
                .map(|r| {
                    let conflicts = if opts.conflicts {
                        r.doc
                            .as_ref()
                            .and_then(|d| d.get("_conflicts"))
                            .and_then(|c| serde_json::from_value::<Vec<String>>(c.clone()).ok())
                            .filter(|c| !c.is_empty())
                    } else {
                        None
                    };
                    ChangeEvent {
                        seq: parse_seq(&r.seq),
                        id: r.id,
                        changes: r
                            .changes
                            .into_iter()
                            .map(|c| ChangeRev { rev: c.rev })
                            .collect(),
                        deleted: r.deleted,
                        doc: if opts.include_docs { r.doc } else { None },
                        conflicts,
                    }
                })
                .collect(),
        })
    }

    async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
        self.ensure_setup().await?;
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
        self.ensure_setup().await?;
        let request = CouchDbBulkGetRequest {
            docs: docs
                .into_iter()
                .map(|d| CouchDbBulkGetDoc {
                    id: d.id,
                    rev: d.rev,
                })
                .collect(),
        };

        // Like PouchDB: inline the attachment bytes (without them a pulled
        // doc only carries stubs and the data never reaches the target) and
        // follow a superseded rev to its latest leaf. JSON is requested
        // explicitly since attachments otherwise make CouchDB reply
        // multipart.
        let resp = self
            .client
            .post(self.url("_bulk_get?revs=true&attachments=true&latest=true"))
            .header(reqwest::header::ACCEPT, "application/json")
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
                            ok: d.ok.map(fill_inline_attachment_lengths),
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
        self.ensure_setup().await?;
        let url = format!(
            "{}/{}?rev={}",
            self.url(&encode_doc_id(doc_id)),
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
        self.ensure_setup().await?;
        let mut url = format!(
            "{}/{}",
            self.url(&encode_doc_id(doc_id)),
            urlencoded(att_id)
        );
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
        self.ensure_setup().await?;
        let url = format!(
            "{}/{}?rev={}",
            self.url(&encode_doc_id(doc_id)),
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
        self.ensure_setup().await?;
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
        self.ensure_setup().await?;
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
        self.ensure_setup().await?;
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
        self.ensure_setup().await?;
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
        self.ensure_setup().await?;
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
        self.ensure_setup().await?;
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
        self.ensure_setup().await?;
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

/// `url` with any `user:password@` removed.
fn url_without_credentials(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.to_string()
        }
        Err(_) => url.to_string(),
    }
}

/// CouchDB omits `length` (and `stub`) on attachments inlined with
/// `attachments=true`; fill in the decoded length so the attachment is not
/// rejected as malformed when the document is parsed.
fn fill_inline_attachment_lengths(mut doc: serde_json::Value) -> serde_json::Value {
    if let Some(atts) = doc.get_mut("_attachments").and_then(|a| a.as_object_mut()) {
        for meta in atts.values_mut() {
            if let Some(meta) = meta.as_object_mut()
                && !meta.contains_key("length")
                && let Some(data) = meta.get("data").and_then(|d| d.as_str())
            {
                let padding = data.bytes().rev().take_while(|&b| b == b'=').count();
                let length = (data.len() / 4 * 3).saturating_sub(padding);
                meta.insert("length".into(), serde_json::json!(length));
            }
        }
    }
    doc
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

/// Encode a document id for use in a CouchDB URL path.
///
/// Like PouchDB's `encodeDocId`, the `/` separating the `_design/` or
/// `_local/` prefix from the rest of the id is kept literal so CouchDB routes
/// design-doc (and `_local`) sub-resources correctly. Without this, an id like
/// `_design/foo` would be encoded to `_design%2Ffoo` and mis-routed.
fn encode_doc_id(id: &str) -> String {
    if let Some(rest) = id.strip_prefix("_design/") {
        format!("_design/{}", urlencoded(rest))
    } else if let Some(rest) = id.strip_prefix("_local/") {
        format!("_local/{}", urlencoded(rest))
    } else {
        urlencoded(id)
    }
}

/// JSON-encode a key value and percent-encode it for safe use as a query
/// parameter (handles `"`, `\`, control chars, `&`, `#`, spaces, unicode).
fn encode_query_key(value: &str) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into());
    urlencoded(&json)
}

#[cfg(test)]
mod tests {
    use super::{
        CouchDbAllDocsResponse, HttpAdapter, encode_doc_id, encode_query_key,
        fill_inline_attachment_lengths, urlencoded,
    };
    use rouchdb_core::adapter::Adapter;

    #[test]
    fn design_and_local_ids_keep_prefix_slash() {
        // The slash after the _design/ or _local/ prefix must stay literal.
        assert_eq!(encode_doc_id("_design/foo"), "_design/foo");
        assert_eq!(encode_doc_id("_local/checkpoint"), "_local/checkpoint");
        // But a slash inside the rest of the id is still encoded.
        assert_eq!(encode_doc_id("_design/a/b"), "_design/a%2Fb");
        // Plain ids: slashes and specials encoded as before.
        assert_eq!(encode_doc_id("a/b"), "a%2Fb");
        assert_eq!(encode_doc_id("user:alice"), urlencoded("user:alice"));
    }

    #[test]
    fn query_keys_are_json_and_url_encoded() {
        // A plain string becomes a JSON-quoted, percent-encoded value.
        assert_eq!(encode_query_key("abc"), "%22abc%22");
        // Special characters that would break a query string are escaped.
        let encoded = encode_query_key("a&b=c #d");
        assert!(!encoded.contains('&'));
        assert!(!encoded.contains('#'));
        assert!(!encoded.contains(' '));
        // Unicode is percent-encoded, not spliced raw.
        let uni = encode_query_key("\u{ffff}");
        assert!(uni.starts_with("%22") && uni.ends_with("%22"));
        assert!(uni.contains('%'));
    }

    #[test]
    fn all_docs_keys_response_decodes() {
        // Verbatim CouchDB 3 reply to `POST _all_docs?include_docs=true` with
        // `{"keys": ["a", "missing"]}`: `offset` is null and the missing key
        // comes back as an error row with no `id` or `value`.
        let body = r#"{"total_rows":1,"offset":null,"rows":[
            {"id":"a","key":"a","value":{"rev":"1-bd51d4cccb23dccbc65c71d8d863ba1c"},"doc":{"_id":"a","_rev":"1-bd51d4cccb23dccbc65c71d8d863ba1c","age":30}},
            {"key":"missing","error":"not_found"}
        ]}"#;
        let resp: CouchDbAllDocsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(resp.offset, None);
        assert_eq!(resp.rows.len(), 2);
        assert_eq!(resp.rows[0].id.as_deref(), Some("a"));
        assert!(resp.rows[1].id.is_none() && resp.rows[1].value.is_none());
    }

    #[test]
    fn bulk_get_inline_attachments_parse() {
        // Verbatim CouchDB 3 `_bulk_get?attachments=true` doc: inline data
        // with no `length` or `stub`.
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"_id":"doc1","_rev":"1-ab5b0978671b42f37de2b4485c8386ff","v":1,
            "_revisions":{"start":1,"ids":["ab5b0978671b42f37de2b4485c8386ff"]},
            "_attachments":{
                "hi.txt":{"content_type":"text/plain","revpos":1,"digest":"md5-O9yO4zjoapsrEQwYrCDNZw==","data":"aGkh"},
                "one.bin":{"content_type":"application/octet-stream","revpos":1,"digest":"md5-x","data":"AQ=="},
                "two.bin":{"content_type":"application/octet-stream","revpos":1,"digest":"md5-y","data":"AQI="}
            }}"#,
        )
        .unwrap();
        let doc = rouchdb_core::document::Document::from_json(fill_inline_attachment_lengths(doc))
            .unwrap();
        assert_eq!(doc.attachments["hi.txt"].data.as_deref(), Some(&b"hi!"[..]));
        assert_eq!(doc.attachments["hi.txt"].length, 3);
        assert_eq!(doc.attachments["one.bin"].length, 1);
        assert_eq!(doc.attachments["two.bin"].length, 2);
    }

    #[tokio::test]
    async fn id_without_server_uuid_is_the_url_without_credentials() {
        // Nothing listens on port 1: the uuid lookup fails fast.
        let a = HttpAdapter::new("http://admin:secret@127.0.0.1:1/userdb");
        let b = HttpAdapter::new("http://127.0.0.1:2/userdb");
        let id_a = a.id().await.unwrap();
        assert_eq!(id_a, "http://127.0.0.1:1/userdb");
        assert_ne!(id_a, b.id().await.unwrap());
    }
}
