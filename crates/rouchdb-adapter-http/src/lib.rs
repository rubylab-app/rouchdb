/// HTTP adapter for RouchDB.
///
/// Communicates with a remote CouchDB-compatible server via HTTP,
/// implementing the Adapter trait by mapping each method to the
/// corresponding CouchDB REST API endpoint.
pub mod auth;
mod request;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, ClientBuilder};
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

/// Default time allowed to establish a connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default time a response may stay silent before the request fails.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Options for [`HttpAdapter::with_options`].
#[derive(Debug, Clone)]
pub struct HttpAdapterOptions {
    /// Do not create the remote database on first use (PouchDB's
    /// `skip_setup`): operations on a missing database fail with NotFound.
    pub skip_setup: bool,
    /// Time allowed to establish a connection.
    pub connect_timeout: Duration,
    /// Time a response may stay silent (no bytes received) before the
    /// request fails, so a stalled server cannot hang a replication forever.
    /// It bounds inactivity, not the total duration of large transfers.
    pub read_timeout: Duration,
}

impl Default for HttpAdapterOptions {
    fn default() -> Self {
        Self {
            skip_setup: false,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
        }
    }
}

/// The reqwest client builder used by default, with the given timeouts.
pub(crate) fn client_builder(connect_timeout: Duration, read_timeout: Duration) -> ClientBuilder {
    Client::builder()
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
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
        let client = client_builder(opts.connect_timeout, opts.read_timeout)
            .build()
            .unwrap_or_default();
        let mut adapter = Self::with_client(url, client);
        adapter.skip_setup = opts.skip_setup;
        adapter
    }

    /// Create a new HTTP adapter with a custom reqwest client. The client's
    /// own timeouts apply (reqwest has none by default).
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
        check_response(response).await
    }
}

/// Map an unsuccessful CouchDB response to a `RouchError`, using the
/// `{"error", "reason"}` body when there is one.
pub(crate) async fn check_response(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().await.unwrap_or_default();
    let couch = serde_json::from_str::<CouchDbError>(&body).ok();
    let reason = |default: &str| {
        couch
            .as_ref()
            .map(|e| e.reason.clone())
            .unwrap_or_else(|| default.to_string())
    };
    Err(match status.as_u16() {
        400 | 413 | 415 => RouchError::BadRequest(reason(&body)),
        401 => RouchError::Unauthorized,
        403 => RouchError::Forbidden(reason("access denied")),
        404 => RouchError::NotFound(reason("missing")),
        409 => RouchError::Conflict,
        // 412 is "file_exists" on database creation, but also e.g.
        // "missing_stub" for a document write.
        412 => match couch {
            Some(ref e) if e.error == "file_exists" => RouchError::DatabaseExists(e.reason.clone()),
            Some(ref e) => RouchError::BadRequest(format!("{}: {}", e.error, e.reason)),
            None => RouchError::BadRequest(body),
        },
        _ => RouchError::DatabaseError(format!("HTTP {}: {}", status, body)),
    })
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
        // databases on different servers do not). A server that answers
        // without a uuid gets the URL without credentials; an unreachable one
        // is an error, so a fallback id is never used by mistake.
        let (server, db) = self
            .base_url
            .rsplit_once('/')
            .unwrap_or((self.base_url.as_str(), ""));
        let resp = self
            .client
            .get(server)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let root: Option<serde_json::Value> = match resp.error_for_status() {
            Ok(resp) => resp.json().await.ok(),
            Err(_) => None,
        };
        Ok(match root.as_ref().and_then(|r| r.get("uuid")?.as_str()) {
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

        // JSON explicitly: with open_revs CouchDB otherwise replies multipart.
        let resp = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let resp = self.check_error(resp).await?;
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        if opts.open_revs.is_some() {
            return winning_open_rev(json, id);
        }
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

/// Pick the document to return from an `open_revs` reply
/// (`[{"ok": doc} | {"missing": rev}]`): like the local adapters, `get`
/// yields one document, the winner among the leaves found (non-deleted
/// first, then highest revision).
fn winning_open_rev(reply: serde_json::Value, id: &str) -> Result<Document> {
    let entries = match reply {
        serde_json::Value::Array(entries) => entries,
        _ => {
            return Err(RouchError::DatabaseError(
                "unexpected open_revs response".into(),
            ));
        }
    };
    entries
        .into_iter()
        .filter_map(|mut entry| entry.get_mut("ok").map(serde_json::Value::take))
        .map(Document::from_json)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max_by(|a, b| (!a.deleted, &a.rev).cmp(&(!b.deleted, &b.rev)))
        .ok_or_else(|| RouchError::NotFound(id.to_string()))
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
        fill_inline_attachment_lengths, urlencoded, winning_open_rev,
    };
    use rouchdb_core::adapter::Adapter;
    use rouchdb_core::error::RouchError;

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
    async fn id_is_server_uuid_plus_db_or_the_url_without_credentials() {
        let (with_uuid, _) = recording_stub_server(json_response(
            "200 OK",
            r#"{"couchdb":"Welcome","uuid":"abc123"}"#,
        ))
        .await;
        let db = HttpAdapter::new(&format!("{with_uuid}/userdb"));
        assert_eq!(db.id().await.unwrap(), "abc123userdb");

        let (no_uuid, _) =
            recording_stub_server(json_response("200 OK", r#"{"couchdb":"Welcome"}"#)).await;
        let url = no_uuid.replace("http://", "http://admin:secret@");
        let db = HttpAdapter::new(&format!("{url}/userdb"));
        assert_eq!(db.id().await.unwrap(), format!("{no_uuid}/userdb"));

        // Nothing listens on port 1: no id rather than a fallback one that
        // would not match the id used once the server is reachable.
        let offline = HttpAdapter::new("http://127.0.0.1:1/userdb");
        assert!(offline.id().await.is_err());
    }

    /// Serve one canned raw HTTP `response` to every connection; returns the
    /// server's base URL.
    pub(crate) async fn stub_server(response: String) -> String {
        recording_stub_server(response).await.0
    }

    /// Like [`stub_server`], also recording each request line
    /// (`"PUT /path HTTP/1.1"`).
    pub(crate) async fn recording_stub_server(
        response: String,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let response = response.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    // Read the request head and body before answering.
                    let mut req = Vec::new();
                    let mut buf = [0u8; 4096];
                    loop {
                        let Ok(n) = socket.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&req).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let len = text
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if req.len() >= end + 4 + len {
                                let line = text.lines().next().unwrap_or_default();
                                recorded.lock().unwrap().push(line.to_string());
                                break;
                            }
                        }
                    }
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{}", addr), requests)
    }

    /// A raw JSON response with the given status line.
    pub(crate) fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// One request received by a [`scripted_server`].
    #[derive(Debug, Clone)]
    pub(crate) struct Captured {
        pub method: String,
        /// Path without the query string.
        pub path: String,
        /// Decoded query parameters, sorted by name.
        pub query: std::collections::BTreeMap<String, String>,
        /// Header names are lowercase.
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Captured {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }

        pub fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body)
                .unwrap_or_else(|e| panic!("{} {} body is not JSON: {e}", self.method, self.path))
        }

        /// `"METHOD /path"`, for comparing request sequences.
        pub fn line(&self) -> String {
            format!("{} {}", self.method, self.path)
        }
    }

    pub(crate) type Requests = std::sync::Arc<std::sync::Mutex<Vec<Captured>>>;

    /// Serve `script` in order, one `(status line, JSON body)` per request,
    /// capturing each request in full. A request beyond the script gets a
    /// 500 whose reason names it, so the test fails with a clear message.
    pub(crate) async fn scripted_server(script: Vec<(&'static str, String)>) -> (String, Requests) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Requests = Default::default();
        let recorded = requests.clone();
        let script = std::sync::Arc::new(std::sync::Mutex::new(
            script
                .into_iter()
                .collect::<std::collections::VecDeque<_>>(),
        ));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let recorded = recorded.clone();
                let script = script.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 8192];
                    let (head_len, body_len) = loop {
                        let Ok(n) = socket.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&buf[..n]);
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..end]).to_string();
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            break (end + 4, len);
                        }
                    };
                    while raw.len() < head_len + body_len {
                        let Ok(n) = socket.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&buf[..n]);
                    }
                    let head = String::from_utf8_lossy(&raw[..head_len - 4]).to_string();
                    let mut lines = head.lines();
                    let mut request_line = lines.next().unwrap_or_default().split(' ');
                    let method = request_line.next().unwrap_or_default().to_string();
                    let target = request_line.next().unwrap_or_default().to_string();
                    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                    let decode = |v: &str| {
                        percent_encoding::percent_decode_str(v)
                            .decode_utf8_lossy()
                            .to_string()
                    };
                    let captured = Captured {
                        method,
                        path: path.to_string(),
                        query: query
                            .split('&')
                            .filter(|p| !p.is_empty())
                            .map(|p| {
                                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                                (decode(k), decode(v))
                            })
                            .collect(),
                        headers: lines
                            .filter_map(|l| l.split_once(':'))
                            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                            .collect(),
                        body: raw[head_len..head_len + body_len].to_vec(),
                    };
                    let line = captured.line();
                    recorded.lock().unwrap().push(captured);
                    let response = match script.lock().unwrap().pop_front() {
                        Some((status, body)) => json_response(status, &body),
                        None => json_response(
                            "500 Internal Server Error",
                            &serde_json::json!({"error": "unscripted", "reason": line}).to_string(),
                        ),
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{}", addr), requests)
    }

    /// An adapter for `{url}/db` that does not probe or create the database.
    fn adapter_at(url: &str) -> HttpAdapter {
        HttpAdapter::with_options(
            &format!("{url}/db"),
            super::HttpAdapterOptions {
                skip_setup: true,
                ..Default::default()
            },
        )
    }

    fn query(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn only_request(requests: &Requests) -> Captured {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "{requests:?}");
        requests[0].clone()
    }

    #[tokio::test]
    async fn changes_asks_for_every_leaf_and_posts_doc_ids() {
        use rouchdb_core::document::{ChangesOptions, ChangesStyle, Seq};
        let (url, requests) = scripted_server(vec![(
            "200 OK",
            r#"{"results":[{"seq":"3-g1AAAA","id":"b","changes":[{"rev":"2-x"},{"rev":"2-y"}],"deleted":true}],"last_seq":7,"pending":0}"#.into(),
        )])
        .await;

        let feed = adapter_at(&url)
            .changes(ChangesOptions {
                since: Seq::Str("1-g1AAAA".into()),
                limit: Some(10),
                style: ChangesStyle::AllDocs,
                doc_ids: Some(vec!["b".into(), "c".into()]),
                ..Default::default()
            })
            .await
            .unwrap();

        let req = only_request(&requests);
        assert_eq!(req.line(), "POST /db/_changes");
        assert_eq!(
            req.query,
            query(&[
                ("since", "1-g1AAAA"),
                ("limit", "10"),
                ("style", "all_docs"),
                ("filter", "_doc_ids"),
            ])
        );
        assert_eq!(req.json(), serde_json::json!({"doc_ids": ["b", "c"]}));

        assert_eq!(feed.last_seq, Seq::Num(7));
        assert_eq!(feed.results.len(), 1);
        let change = &feed.results[0];
        assert_eq!(change.seq, Seq::Str("3-g1AAAA".into()));
        assert_eq!(change.id, "b");
        let revs: Vec<&str> = change.changes.iter().map(|c| c.rev.as_str()).collect();
        assert_eq!(revs, vec!["2-x", "2-y"]);
        assert!(change.deleted);
        assert!(change.doc.is_none() && change.conflicts.is_none());
    }

    #[tokio::test]
    async fn changes_conflicts_fetch_docs_but_do_not_return_them() {
        use rouchdb_core::document::ChangesOptions;
        let (url, requests) = scripted_server(vec![(
            "200 OK",
            r#"{"results":[{"seq":2,"id":"d","changes":[{"rev":"2-b"}],"doc":{"_id":"d","_rev":"2-b","_conflicts":["2-a"]}}],"last_seq":2}"#.into(),
        )])
        .await;

        let feed = adapter_at(&url)
            .changes(ChangesOptions {
                conflicts: true,
                selector: Some(serde_json::json!({"type": "t"})),
                ..Default::default()
            })
            .await
            .unwrap();

        // CouchDB only reports conflicts inside the docs.
        let req = only_request(&requests);
        assert_eq!(req.line(), "POST /db/_changes");
        assert_eq!(
            req.query,
            query(&[
                ("since", "0"),
                ("include_docs", "true"),
                ("conflicts", "true"),
                ("filter", "_selector"),
            ])
        );
        assert_eq!(req.json(), serde_json::json!({"selector": {"type": "t"}}));
        assert_eq!(feed.results[0].conflicts, Some(vec!["2-a".to_string()]));
        assert!(feed.results[0].doc.is_none());
    }

    #[tokio::test]
    async fn plain_changes_is_a_get_with_only_since() {
        use rouchdb_core::document::{ChangesOptions, Seq};
        let (url, requests) = scripted_server(vec![(
            "200 OK",
            r#"{"results":[],"last_seq":"0-g1AAAA"}"#.into(),
        )])
        .await;
        let feed = adapter_at(&url)
            .changes(ChangesOptions::default())
            .await
            .unwrap();
        let req = only_request(&requests);
        assert_eq!(req.line(), "GET /db/_changes");
        assert_eq!(req.query, query(&[("since", "0")]));
        assert_eq!(feed.last_seq, Seq::Str("0-g1AAAA".into()));
    }

    fn replicated_doc() -> rouchdb_core::document::Document {
        rouchdb_core::document::Document::from_json(serde_json::json!({
            "_id": "d",
            "_rev": "2-b",
            "_revisions": {"start": 2, "ids": ["b", "a"]},
            "v": 1,
            "_attachments": {"a.bin": {"content_type": "application/octet-stream", "data": "AAH/"}}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn bulk_docs_in_replication_mode_sends_new_edits_false() {
        use rouchdb_core::document::BulkDocsOptions;
        let (url, requests) = scripted_server(vec![
            // CouchDB lists only failures when new_edits is false.
            ("201 Created", "[]".into()),
            (
                "201 Created",
                r#"[{"id":"d","error":"forbidden","reason":"no"}]"#.into(),
            ),
        ])
        .await;
        let db = adapter_at(&url);

        let stored = db
            .bulk_docs(vec![replicated_doc()], BulkDocsOptions::replication())
            .await
            .unwrap();
        assert!(stored.is_empty(), "{stored:?}");
        let denied = db
            .bulk_docs(vec![replicated_doc()], BulkDocsOptions::replication())
            .await
            .unwrap();
        assert_eq!(denied.len(), 1);
        assert!(!denied[0].ok);
        assert_eq!(denied[0].id, "d");
        assert_eq!(denied[0].error.as_deref(), Some("forbidden"));
        assert_eq!(denied[0].reason.as_deref(), Some("no"));

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line(), "POST /db/_bulk_docs");
        let body = requests[0].json();
        assert_eq!(body["new_edits"], false);
        let doc = &body["docs"][0];
        assert_eq!(doc["_rev"], "2-b");
        assert_eq!(
            doc["_revisions"],
            serde_json::json!({"start": 2, "ids": ["b", "a"]})
        );
        assert_eq!(doc["_attachments"]["a.bin"]["data"], "AAH/");
        assert_eq!(doc["v"], 1);
    }

    #[tokio::test]
    async fn bulk_docs_with_new_edits_reports_each_doc() {
        use rouchdb_core::document::{BulkDocsOptions, Document};
        let (url, requests) = scripted_server(vec![(
            "201 Created",
            r#"[{"ok":true,"id":"n","rev":"1-a"},{"id":"x","rev":"1-b","error":"conflict","reason":"Document update conflict."}]"#.into(),
        )])
        .await;
        let docs = ["n", "x"]
            .map(|id| Document::from_json(serde_json::json!({"_id": id})).unwrap())
            .to_vec();

        let results = adapter_at(&url)
            .bulk_docs(docs, BulkDocsOptions::new())
            .await
            .unwrap();

        let body = only_request(&requests).json();
        assert!(body.get("new_edits").is_none(), "{body}");
        assert_eq!(body["docs"].as_array().unwrap().len(), 2);
        let summary: Vec<_> = results
            .iter()
            .map(|r| (r.ok, r.id.as_str(), r.rev.as_deref(), r.error.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (true, "n", Some("1-a"), None),
                (false, "x", Some("1-b"), Some("conflict")),
            ]
        );
    }

    #[tokio::test]
    async fn bulk_get_asks_for_history_attachments_and_latest() {
        use rouchdb_core::document::BulkGetItem;
        let (url, requests) = scripted_server(vec![(
            "200 OK",
            r#"{"results":[
                {"id":"d","docs":[{"ok":{"_id":"d","_rev":"2-b","_revisions":{"start":2,"ids":["b","a"]},
                    "_attachments":{
                        "three.txt":{"content_type":"text/plain","revpos":2,"digest":"md5-x","data":"aGkh"},
                        "one.bin":{"content_type":"application/octet-stream","revpos":2,"digest":"md5-y","data":"AQ=="},
                        "two.bin":{"content_type":"application/octet-stream","revpos":2,"digest":"md5-z","data":"AQI="},
                        "sized.bin":{"content_type":"application/octet-stream","revpos":2,"digest":"md5-w","data":"AQI=","length":2}}}}]},
                {"id":"e","docs":[{"error":{"id":"e","rev":"1-x","error":"not_found","reason":"missing"}}]}
            ]}"#.into(),
        )])
        .await;

        let resp = adapter_at(&url)
            .bulk_get(vec![
                BulkGetItem {
                    id: "d".into(),
                    rev: Some("2-b".into()),
                },
                BulkGetItem {
                    id: "e".into(),
                    rev: None,
                },
            ])
            .await
            .unwrap();

        let req = only_request(&requests);
        assert_eq!(req.line(), "POST /db/_bulk_get");
        assert_eq!(
            req.query,
            query(&[
                ("revs", "true"),
                ("attachments", "true"),
                ("latest", "true")
            ])
        );
        // JSON, not the multipart reply CouchDB sends by default here.
        assert_eq!(req.header("accept"), Some("application/json"));
        assert_eq!(
            req.json(),
            serde_json::json!({"docs": [{"id": "d", "rev": "2-b"}, {"id": "e"}]})
        );

        // CouchDB omits `length` for inline data: it is filled in.
        let doc = resp.results[0].docs[0].ok.as_ref().unwrap();
        let lengths: Vec<_> = ["three.txt", "one.bin", "two.bin", "sized.bin"]
            .iter()
            .map(|a| doc["_attachments"][a]["length"].clone())
            .collect();
        assert_eq!(lengths, [3, 1, 2, 2].map(|n| serde_json::json!(n)));
        let err = resp.results[1].docs[0].error.as_ref().unwrap();
        assert_eq!(
            (
                err.id.as_str(),
                err.rev.as_str(),
                err.error.as_str(),
                err.reason.as_str()
            ),
            ("e", "1-x", "not_found", "missing")
        );
    }

    #[tokio::test]
    async fn all_docs_sends_only_the_requested_options() {
        use rouchdb_core::document::AllDocsOptions;
        let reply = r#"{"total_rows":0,"offset":0,"rows":[]}"#;
        let (url, requests) = scripted_server(vec![
            ("200 OK", reply.into()),
            ("200 OK", reply.into()),
            ("200 OK", reply.into()),
        ])
        .await;
        let db = adapter_at(&url);

        db.all_docs(AllDocsOptions::new()).await.unwrap();
        db.all_docs(AllDocsOptions {
            include_docs: true,
            limit: Some(2),
            skip: 1,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
        db.all_docs(AllDocsOptions {
            keys: Some(vec!["a".into(), "b".into()]),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line(), "GET /db/_all_docs");
        assert_eq!(requests[0].query, query(&[]));
        assert_eq!(
            requests[1].query,
            query(&[("include_docs", "true"), ("limit", "2"), ("skip", "1")])
        );
        assert_eq!(requests[2].line(), "POST /db/_all_docs");
        assert_eq!(requests[2].json(), serde_json::json!({"keys": ["a", "b"]}));
    }

    #[tokio::test]
    async fn remove_local_deletes_the_current_rev() {
        let (url, requests) = scripted_server(vec![
            (
                "200 OK",
                r#"{"_id":"_local/cp","_rev":"0-3","last_seq":5}"#.into(),
            ),
            (
                "200 OK",
                r#"{"ok":true,"id":"_local/cp","rev":"0-0"}"#.into(),
            ),
            (
                "404 Object Not Found",
                r#"{"error":"not_found","reason":"missing"}"#.into(),
            ),
        ])
        .await;
        let db = adapter_at(&url);

        db.remove_local("cp").await.unwrap();
        let missing = db.remove_local("cp").await;
        assert!(
            matches!(missing, Err(RouchError::NotFound(ref r)) if r == "missing"),
            "{missing:?}"
        );

        let requests = requests.lock().unwrap();
        let lines: Vec<_> = requests.iter().map(Captured::line).collect();
        assert_eq!(
            lines,
            vec![
                "GET /db/_local/cp",
                "DELETE /db/_local/cp",
                "GET /db/_local/cp"
            ]
        );
        assert_eq!(requests[1].query, query(&[("rev", "0-3")]));
    }

    #[tokio::test]
    async fn compact_posts_json_and_reports_errors() {
        let (url, requests) = scripted_server(vec![
            ("202 Accepted", r#"{"ok":true}"#.into()),
            (
                "401 Unauthorized",
                r#"{"error":"unauthorized","reason":"You are not a server admin."}"#.into(),
            ),
        ])
        .await;
        let db = adapter_at(&url);

        db.compact().await.unwrap();
        assert!(matches!(db.compact().await, Err(RouchError::Unauthorized)));

        let req = requests.lock().unwrap()[0].clone();
        assert_eq!(req.line(), "POST /db/_compact");
        assert_eq!(req.header("content-type"), Some("application/json"));
    }

    #[tokio::test]
    async fn security_document_round_trips_over_http() {
        use rouchdb_core::document::SecurityDocument;
        let security = serde_json::json!({
            "admins": {"names": ["ann"], "roles": ["ops"]},
            "members": {"names": [], "roles": ["staff"]},
        });
        let (url, requests) = scripted_server(vec![
            ("200 OK", security.to_string()),
            ("200 OK", r#"{"ok":true}"#.into()),
            (
                "403 Forbidden",
                r#"{"error":"forbidden","reason":"You are not a db or server admin."}"#.into(),
            ),
        ])
        .await;
        let db = adapter_at(&url);

        let doc = db.get_security().await.unwrap();
        assert_eq!(serde_json::to_value(&doc).unwrap(), security);
        db.put_security(doc).await.unwrap();
        let denied = db.put_security(SecurityDocument::default()).await;
        assert!(
            matches!(denied, Err(RouchError::Forbidden(ref r)) if r == "You are not a db or server admin."),
            "{denied:?}"
        );

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line(), "GET /db/_security");
        assert_eq!(requests[1].line(), "PUT /db/_security");
        assert_eq!(requests[1].json(), security);
    }

    #[tokio::test]
    async fn missing_database_is_created_on_first_use() {
        let info = r#"{"db_name":"db","doc_count":0,"doc_del_count":0,"update_seq":"0-g1AAAA"}"#;
        let (url, requests) = scripted_server(vec![
            (
                "404 Object Not Found",
                r#"{"error":"not_found","reason":"Database does not exist."}"#.into(),
            ),
            ("201 Created", r#"{"ok":true}"#.into()),
            ("200 OK", info.into()),
            ("200 OK", info.into()),
        ])
        .await;
        let db = HttpAdapter::new(&format!("{url}/db"));

        db.info().await.unwrap();
        db.info().await.unwrap();

        let lines: Vec<_> = requests
            .lock()
            .unwrap()
            .iter()
            .map(Captured::line)
            .collect();
        assert_eq!(lines, vec!["GET /db", "PUT /db", "GET /db", "GET /db"]);
    }

    #[tokio::test]
    async fn existing_database_is_not_created_again() {
        let info = r#"{"db_name":"db","doc_count":1,"doc_del_count":0,"update_seq":7}"#;
        let (url, requests) =
            scripted_server(vec![("200 OK", info.into()), ("200 OK", info.into())]).await;
        let db = HttpAdapter::new(&format!("{url}/db"));

        let got = db.info().await.unwrap();
        assert_eq!(got.update_seq, rouchdb_core::document::Seq::Num(7));
        let lines: Vec<_> = requests
            .lock()
            .unwrap()
            .iter()
            .map(Captured::line)
            .collect();
        assert_eq!(lines, vec!["GET /db", "GET /db"]);
    }

    #[tokio::test]
    async fn get_sends_only_the_requested_options() {
        use rouchdb_core::document::GetOptions;
        let doc = r#"{"_id":"a/b","_rev":"2-x","v":1,"_conflicts":["2-w"]}"#;
        let (url, requests) =
            scripted_server(vec![("200 OK", doc.into()), ("200 OK", doc.into())]).await;
        let db = adapter_at(&url);

        db.get("a/b", GetOptions::default()).await.unwrap();
        let got = db
            .get(
                "a/b",
                GetOptions {
                    conflicts: true,
                    revs: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(got.data["_conflicts"], serde_json::json!(["2-w"]));

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line(), "GET /db/a%2Fb");
        assert_eq!(requests[0].query, query(&[]));
        assert_eq!(
            requests[1].query,
            query(&[("conflicts", "true"), ("revs", "true")])
        );
    }

    #[tokio::test]
    async fn get_attachment_returns_the_raw_bytes() {
        use rouchdb_core::document::GetAttachmentOptions;
        // Returned as is, not parsed as JSON.
        let body = "\u{0}raw\u{7f} bytes";
        let (url, requests) = scripted_server(vec![
            ("200 OK", body.into()),
            (
                "404 Object Not Found",
                r#"{"error":"not_found","reason":"Document is missing attachment"}"#.into(),
            ),
        ])
        .await;
        let db = adapter_at(&url);

        let got = db
            .get_attachment(
                "d",
                "a b.txt",
                GetAttachmentOptions {
                    rev: Some("1-x".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(got, body.as_bytes());
        let missing = db
            .get_attachment("d", "gone", GetAttachmentOptions::default())
            .await;
        assert!(
            matches!(missing, Err(RouchError::NotFound(ref r)) if r == "Document is missing attachment"),
            "{missing:?}"
        );

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line(), "GET /db/d/a%20b.txt");
        assert_eq!(requests[0].query, query(&[("rev", "1-x")]));
        assert_eq!(requests[1].line(), "GET /db/d/gone");
    }

    #[tokio::test]
    async fn put_local_sends_the_doc_and_reports_errors() {
        let (url, requests) = scripted_server(vec![
            (
                "201 Created",
                r#"{"ok":true,"id":"_local/cp","rev":"0-1"}"#.into(),
            ),
            (
                "400 Bad Request",
                r#"{"error":"bad_request","reason":"Invalid rev format"}"#.into(),
            ),
        ])
        .await;
        let db = adapter_at(&url);

        db.put_local("cp", serde_json::json!({"last_seq": 5}))
            .await
            .unwrap();
        let bad = db
            .put_local("cp", serde_json::json!({"_rev": "x", "last_seq": 6}))
            .await;
        assert!(
            matches!(bad, Err(RouchError::BadRequest(ref r)) if r == "Invalid rev format"),
            "{bad:?}"
        );

        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].line(), "PUT /db/_local/cp");
        assert_eq!(requests[0].json(), serde_json::json!({"last_seq": 5}));
    }

    #[tokio::test]
    async fn destroy_deletes_the_database_and_reports_errors() {
        let (url, requests) = scripted_server(vec![
            ("200 OK", r#"{"ok":true}"#.into()),
            (
                "404 Object Not Found",
                r#"{"error":"not_found","reason":"Database does not exist."}"#.into(),
            ),
        ])
        .await;
        let db = adapter_at(&url);

        db.destroy().await.unwrap();
        assert!(matches!(db.destroy().await, Err(RouchError::NotFound(_))));
        let lines: Vec<_> = requests
            .lock()
            .unwrap()
            .iter()
            .map(Captured::line)
            .collect();
        assert_eq!(lines, vec!["DELETE /db", "DELETE /db"]);
    }

    #[tokio::test]
    async fn adapter_from_an_auth_client_sends_its_session_cookie() {
        let info = r#"{"db_name":"db","doc_count":0,"doc_del_count":0,"update_seq":0}"#;
        let (url, requests) = scripted_server(vec![
            // A status line can carry extra headers after it.
            (
                "200 OK\r\nSet-Cookie: AuthSession=c2Vzc2lvbg; Version=1; Path=/; HttpOnly",
                r#"{"ok":true,"name":"bob","roles":[]}"#.into(),
            ),
            ("200 OK", info.into()),
            ("200 OK", info.into()),
        ])
        .await;
        let auth = super::auth::AuthClient::new(&url);
        auth.login("bob", "secret").await.unwrap();

        let db = HttpAdapter::with_auth_client(&format!("{url}/db"), &auth);
        db.info().await.unwrap();

        let requests = requests.lock().unwrap();
        let lines: Vec<_> = requests.iter().map(Captured::line).collect();
        assert_eq!(lines, vec!["POST /_session", "GET /db", "GET /db"]);
        for req in &requests[1..] {
            assert_eq!(req.header("cookie"), Some("AuthSession=c2Vzc2lvbg"));
        }
    }

    #[tokio::test]
    async fn database_created_concurrently_is_not_an_error() {
        let (url, requests) = scripted_server(vec![
            (
                "404 Object Not Found",
                r#"{"error":"not_found","reason":"Database does not exist."}"#.into(),
            ),
            (
                "412 Precondition Failed",
                r#"{"error":"file_exists","reason":"The database could not be created, the file already exists."}"#.into(),
            ),
            (
                "200 OK",
                r#"{"db_name":"db","doc_count":0,"doc_del_count":0,"update_seq":"0-g1AAAA"}"#.into(),
            ),
        ])
        .await;
        HttpAdapter::new(&format!("{url}/db")).info().await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    async fn error_for(status: &str, body: &str) -> rouchdb_core::error::RouchError {
        let url = stub_server(json_response(status, body)).await;
        let db = HttpAdapter::with_options(
            &format!("{url}/db"),
            super::HttpAdapterOptions {
                skip_setup: true,
                ..Default::default()
            },
        );
        db.info().await.unwrap_err()
    }

    #[tokio::test]
    async fn http_errors_map_to_rouch_errors() {
        let err = error_for(
            "400 Bad Request",
            r#"{"error":"bad_request","reason":"Invalid rev format"}"#,
        )
        .await;
        assert!(
            matches!(err, RouchError::BadRequest(ref r) if r == "Invalid rev format"),
            "{err:?}"
        );

        let err = error_for(
            "412 Precondition Failed",
            r#"{"error":"file_exists","reason":"The database could not be created, the file already exists."}"#,
        )
        .await;
        assert!(matches!(err, RouchError::DatabaseExists(_)), "{err:?}");

        let err = error_for(
            "412 Precondition Failed",
            r#"{"error":"missing_stub","reason":"Invalid attachment stub in d for a.txt"}"#,
        )
        .await;
        assert!(
            matches!(err, RouchError::BadRequest(ref r) if r.contains("missing_stub")),
            "{err:?}"
        );

        let err = error_for(
            "413 Request Entity Too Large",
            r#"{"error":"document_too_large","reason":"d"}"#,
        )
        .await;
        assert!(matches!(err, RouchError::BadRequest(_)), "{err:?}");

        let err = error_for(
            "415 Unsupported Media Type",
            r#"{"error":"bad_content_type","reason":"Content-Type must be application/json"}"#,
        )
        .await;
        assert!(matches!(err, RouchError::BadRequest(_)), "{err:?}");

        let err = error_for("403 Forbidden", r#"{"error":"forbidden","reason":"no"}"#).await;
        assert!(
            matches!(err, RouchError::Forbidden(ref r) if r == "no"),
            "{err:?}"
        );

        let err = error_for(
            "404 Object Not Found",
            r#"{"error":"not_found","reason":"Database does not exist."}"#,
        )
        .await;
        assert!(
            matches!(err, RouchError::NotFound(ref r) if r == "Database does not exist."),
            "{err:?}"
        );
        // Without a CouchDB error body the reason falls back to "missing".
        let err = error_for("404 Not Found", "nope").await;
        assert!(
            matches!(err, RouchError::NotFound(ref r) if r == "missing"),
            "{err:?}"
        );

        let err = error_for(
            "401 Unauthorized",
            r#"{"error":"unauthorized","reason":"Name or password is incorrect."}"#,
        )
        .await;
        assert!(matches!(err, RouchError::Unauthorized), "{err:?}");

        let err = error_for(
            "409 Conflict",
            r#"{"error":"conflict","reason":"Document update conflict."}"#,
        )
        .await;
        assert!(matches!(err, RouchError::Conflict), "{err:?}");

        let err = error_for(
            "412 Precondition Failed",
            r#"{"error":"missing_stub","reason":"Invalid attachment stub in d for a.txt"}"#,
        )
        .await;
        assert!(
            matches!(err, RouchError::BadRequest(ref r) if r == "missing_stub: Invalid attachment stub in d for a.txt"),
            "{err:?}"
        );

        let body = r#"{"error":"unknown_error","reason":"function_clause"}"#;
        let err = error_for("500 Internal Server Error", body).await;
        assert!(
            matches!(err, RouchError::DatabaseError(ref r) if *r == format!("HTTP 500 Internal Server Error: {body}")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn login_with_bad_credentials_is_unauthorized() {
        let url = stub_server(json_response(
            "401 Unauthorized",
            r#"{"error":"unauthorized","reason":"Name or password is incorrect."}"#,
        ))
        .await;
        let auth = super::auth::AuthClient::new(&url);
        let err = auth.login("bob", "wrong").await.unwrap_err();
        assert!(
            matches!(err, rouchdb_core::error::RouchError::Unauthorized),
            "{err:?}"
        );
    }

    /// A server that accepts connections and never answers.
    async fn hung_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                open.push(socket);
            }
        });
        format!("http://{}/db", addr)
    }

    #[tokio::test]
    async fn stalled_server_times_out() {
        let db = HttpAdapter::with_options(
            &hung_server().await,
            super::HttpAdapterOptions {
                skip_setup: true,
                read_timeout: std::time::Duration::from_millis(200),
                ..Default::default()
            },
        );
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), db.info())
            .await
            .expect("request to a stalled server never timed out");
        assert!(result.is_err());
    }

    #[test]
    fn open_revs_reply_yields_the_winning_leaf() {
        // CouchDB `GET /db/d?open_revs=all` with Accept: application/json.
        let reply: serde_json::Value = serde_json::from_str(
            r#"[{"ok":{"_id":"d","_rev":"2-bbb","v":"b"}},
                {"ok":{"_id":"d","_rev":"3-ddd","_deleted":true}},
                {"missing":"4-eee"}]"#,
        )
        .unwrap();
        let doc = winning_open_rev(reply, "d").unwrap();
        assert_eq!(doc.rev.unwrap().to_string(), "2-bbb");

        let none: serde_json::Value = serde_json::from_str(r#"[{"missing":"1-x"}]"#).unwrap();
        assert!(matches!(
            winning_open_rev(none, "d"),
            Err(rouchdb_core::error::RouchError::NotFound(_))
        ));
    }
}
