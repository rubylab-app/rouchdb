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
use rouchdb_core::json::MAX_NESTING_DEPTH;

/// Containers a CouchDB response wraps around a document (a `_bulk_get`
/// result: object, `results`, result, `docs`, entry), with some margin.
const RESPONSE_ENVELOPE_DEPTH: usize = 8;

/// Decode a JSON response body. Documents in it may be nested as deep as
/// rouchdb stores them ([`MAX_NESTING_DEPTH`]); serde_json alone stops at
/// 128 levels, which made deep CouchDB documents unreadable.
pub(crate) fn decode_response<T>(bytes: &[u8]) -> Result<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    rouchdb_core::json::from_slice(bytes, MAX_NESTING_DEPTH + RESPONSE_ENVELOPE_DEPTH)
        .map_err(|e| RouchError::DatabaseError(e.to_string()))
}

/// Read and decode a JSON response body (see [`decode_response`]).
async fn read_json<T>(resp: reqwest::Response) -> Result<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
    decode_response(&bytes)
}

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
    /// When `keys` are posted, keys that do not exist come back as
    /// `{"key": "x", "error": "not_found"}` rows, which `AllDocsRow` keeps.
    rows: Vec<AllDocsRow>,
    /// Present when `update_seq=true` was requested.
    #[serde(default)]
    update_seq: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// HttpAdapter
// ---------------------------------------------------------------------------

/// Default time allowed to establish a connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default time a response may stay silent before the request fails.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Options for [`HttpAdapter::with_options`].
///
/// Set the options you need and fill the rest with `..Default::default()`:
/// fields may be added in minor releases, and a literal that lists every
/// field would then stop compiling.
///
/// ```
/// use rouchdb_adapter_http::{HttpAdapter, HttpAdapterOptions};
///
/// let adapter = HttpAdapter::with_options(
///     "http://localhost:5984/mydb",
///     HttpAdapterOptions {
///         skip_setup: true,
///         ..Default::default()
///     },
/// );
/// # let _ = adapter;
/// ```
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
    /// Set once the remote database is known to exist; replaced by a fresh
    /// cell when the database is destroyed, so it is created again on the
    /// next use.
    setup: std::sync::Mutex<std::sync::Arc<tokio::sync::OnceCell<()>>>,
    /// The id derived from the server's uuid, once known: it does not change
    /// while the server runs, and live replication asks for it every pass.
    id: tokio::sync::OnceCell<String>,
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
            setup: Default::default(),
            id: Default::default(),
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
        let setup = self
            .setup
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        setup
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
        // A malformed revision, as the local adapters report it.
        400 if couch
            .as_ref()
            .is_some_and(|e| e.reason == "Invalid rev format") =>
        {
            RouchError::InvalidRev(reason(&body))
        }
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
/// A `DocResult` exactly as CouchDB reported it (any member may be missing).
fn wire_doc_result(
    ok: bool,
    id: String,
    rev: Option<String>,
    error: Option<String>,
    reason: Option<String>,
) -> DocResult {
    let mut result = DocResult::ok(id, "");
    result.ok = ok;
    result.rev = rev;
    result.error = error;
    result.reason = reason;
    result
}

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
        let info: CouchDbInfo = read_json(resp).await?;

        Ok(DbInfo::new(
            info.db_name,
            info.doc_count,
            info.doc_del_count,
            parse_seq(&info.update_seq),
        ))
    }

    async fn id(&self) -> Result<String> {
        // Like PouchDB: the server's uuid plus the database name, so every
        // URL of the same database maps to one replication id (and same-named
        // databases on different servers do not). A server that answers
        // without a uuid gets the URL without credentials; an unreachable one
        // is an error, so a fallback id is never used by mistake. Only the
        // uuid-based id is cached.
        if let Some(id) = self.id.get() {
            return Ok(id.clone());
        }
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
            Some(uuid) => {
                let id = format!("{}{}", uuid, db);
                let _ = self.id.set(id.clone());
                id
            }
            None => url_without_credentials(&self.base_url),
        })
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        self.ensure_setup().await?;
        let mut url = self.url(&encode_doc_id(id));
        let mut params = Vec::new();

        if let Some(ref rev) = opts.rev {
            params.push(format!("rev={}", urlencoded(rev)));
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
        let json: serde_json::Value = read_json(resp).await?;

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
        // A document nested deeper than rouchdb stores them is rejected on
        // its own, as the local adapters do: CouchDB would accept it, but
        // it could not be read back.
        let mut rejected: Vec<Option<DocResult>> = Vec::with_capacity(docs.len());
        let mut json_docs = Vec::with_capacity(docs.len());
        for doc in &docs {
            match rouchdb_core::json::check_document_depth(&doc.data) {
                Ok(()) => {
                    rejected.push(None);
                    let mut json = doc.to_json();
                    // Without an id the server generates one.
                    if doc.id.is_empty()
                        && let Some(obj) = json.as_object_mut()
                    {
                        obj.remove("_id");
                    }
                    json_docs.push(json);
                }
                Err(e) => {
                    let reason = match e {
                        RouchError::BadRequest(reason) => reason,
                        other => other.to_string(),
                    };
                    rejected.push(Some(rouchdb_core::write::error_result(
                        &doc.id,
                        "bad_request",
                        &reason,
                    )));
                }
            }
        }

        let results: Vec<CouchDbBulkDocsResult> = if json_docs.is_empty() {
            Vec::new()
        } else {
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
            read_json(resp).await?
        };
        let mut results = results.into_iter().map(|r| {
            wire_doc_result(
                r.ok.unwrap_or(r.error.is_none()),
                r.id.unwrap_or_default(),
                r.rev,
                r.error,
                r.reason,
            )
        });

        if !opts.new_edits {
            // CouchDB lists only the documents it could not write.
            let mut failed: Vec<DocResult> = rejected.into_iter().flatten().collect();
            failed.extend(results);
            return Ok(failed);
        }
        // One result per document, in order.
        Ok(rejected
            .into_iter()
            .filter_map(|r| r.or_else(|| results.next()))
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
        let result: CouchDbAllDocsResponse = read_json(resp).await?;

        Ok(
            AllDocsResponse::new(result.total_rows, result.offset.unwrap_or(0), result.rows)
                .with_update_seq(result.update_seq.as_ref().map(parse_seq)),
        )
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
        let result: CouchDbChangesResponse = read_json(resp).await?;

        Ok(ChangesResponse::new(
            result
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
                    ChangeEvent::new(
                        parse_seq(&r.seq),
                        r.id,
                        r.changes.into_iter().map(|c| c.rev),
                    )
                    .with_deleted(r.deleted)
                    .with_doc(if opts.include_docs { r.doc } else { None })
                    .with_conflicts(conflicts)
                })
                .collect(),
            parse_seq(&result.last_seq),
        ))
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

        let results: HashMap<String, RevsDiffResult> = read_json(resp).await?;

        Ok(RevsDiffResponse::new(results))
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

        let result: CouchDbBulkGetResponse = read_json(resp).await?;

        Ok(BulkGetResponse::new(
            result
                .results
                .into_iter()
                .map(|r| {
                    let docs = r
                        .docs
                        .into_iter()
                        .map(|d| {
                            // As CouchDB sent it (either member may be missing).
                            let mut doc = BulkGetDoc::ok(serde_json::Value::Null);
                            doc.ok = d.ok.map(fill_inline_attachment_lengths);
                            doc.error = d
                                .error
                                .map(|e| BulkGetError::new(e.id, e.rev, e.error, e.reason));
                            doc
                        })
                        .collect();
                    BulkGetResult::new(r.id, docs)
                })
                .collect(),
        ))
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
        let result: CouchDbPutResponse = read_json(resp).await?;

        Ok(wire_doc_result(
            result.ok.unwrap_or(true),
            result.id,
            Some(result.rev),
            None,
            None,
        ))
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
        let result: CouchDbPutResponse = read_json(resp).await?;

        Ok(wire_doc_result(
            result.ok.unwrap_or(true),
            result.id,
            Some(result.rev),
            None,
            None,
        ))
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
        let json: serde_json::Value = read_json(resp).await?;
        Ok(json)
    }

    async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
        rouchdb_core::json::check_document_depth(&doc)?;
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

    /// Delete the remote database. Like PouchDB, a database that does not
    /// exist is not an error, and (unless `skip_setup` is set) the next
    /// operation creates it again, empty, as the local adapters behave.
    async fn destroy(&self) -> Result<()> {
        let resp = self
            .client
            .delete(&self.base_url)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        if resp.status() != reqwest::StatusCode::NOT_FOUND {
            self.check_error(resp).await?;
        }
        *self
            .setup
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Default::default();
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
        let result: PurgeResponse = read_json(resp).await?;
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
        let doc: SecurityDocument = read_json(resp).await?;
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
        let (with_uuid, requests) = recording_stub_server(json_response(
            "200 OK",
            r#"{"couchdb":"Welcome","uuid":"abc123"}"#,
        ))
        .await;
        let db = HttpAdapter::new(&format!("{with_uuid}/userdb"));
        assert_eq!(db.id().await.unwrap(), "abc123userdb");
        // The uuid-based id is cached (live replication asks every pass).
        assert_eq!(db.id().await.unwrap(), "abc123userdb");
        assert_eq!(requests.lock().unwrap().len(), 1);

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

    /// A document without an id is sent without `_id`, so the server
    /// generates one, as the local adapters do (CouchDB rejects `"_id": ""`).
    #[tokio::test]
    async fn bulk_docs_leaves_missing_ids_to_the_server() {
        use rouchdb_core::document::{BulkDocsOptions, Document};
        let (url, requests) = scripted_server(vec![(
            "201 Created",
            r#"[{"ok":true,"id":"39957e80528575124dadd8d248004e77","rev":"1-a"},{"ok":true,"id":"x","rev":"1-b"}]"#.into(),
        )])
        .await;
        let docs = vec![
            Document::from_json(serde_json::json!({"v": 1})).unwrap(),
            Document::from_json(serde_json::json!({"_id": "x"})).unwrap(),
        ];
        assert_eq!(docs[0].id, "");
        let results = adapter_at(&url)
            .bulk_docs(docs, BulkDocsOptions::new())
            .await
            .unwrap();
        assert_eq!(results[0].id, "39957e80528575124dadd8d248004e77");
        let body = only_request(&requests).json();
        assert_eq!(body["docs"], serde_json::json!([{"v": 1}, {"_id": "x"}]));
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

    /// `{"_id": id, "v": [[...[1]...]]}`, `depth` containers deep.
    fn nested_doc(id: &str, depth: usize) -> rouchdb_core::document::Document {
        let mut v = serde_json::json!(1);
        for _ in 1..depth {
            v = serde_json::Value::Array(vec![v]);
        }
        rouchdb_core::document::Document::from_json(serde_json::json!({"_id": id, "v": v})).unwrap()
    }

    /// Like the local adapters, a document nested deeper than rouchdb stores
    /// them is rejected on its own (CouchDB would store it, but rouchdb
    /// could not read it back); the others are written.
    #[tokio::test]
    async fn bulk_docs_rejects_too_deep_documents_like_local_adapters() {
        use super::MAX_NESTING_DEPTH;
        use rouchdb_core::document::BulkDocsOptions;
        let (url, requests) = scripted_server(vec![
            (
                "201 Created",
                r#"[{"ok":true,"id":"a","rev":"1-a"},{"ok":true,"id":"c","rev":"1-c"}]"#.into(),
            ),
            ("201 Created", "[]".into()),
        ])
        .await;
        let db = adapter_at(&url);
        let docs = vec![
            nested_doc("a", MAX_NESTING_DEPTH),
            nested_doc("b", MAX_NESTING_DEPTH + 1),
            nested_doc("c", 1),
        ];
        let results = db.bulk_docs(docs, BulkDocsOptions::new()).await.unwrap();
        let reason = format!("Document nesting exceeds the maximum depth of {MAX_NESTING_DEPTH}");
        let summary: Vec<_> = results
            .iter()
            .map(|r| (r.ok, r.id.as_str(), r.error.as_deref(), r.reason.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (true, "a", None, None),
                (false, "b", Some("bad_request"), Some(reason.as_str())),
                (true, "c", None, None),
            ]
        );

        // Replication mode: CouchDB lists only failures.
        let mut too_deep = nested_doc("r", MAX_NESTING_DEPTH + 1);
        too_deep.rev = Some("1-abc".parse().unwrap());
        let results = db
            .bulk_docs(
                vec![too_deep, replicated_doc()],
                BulkDocsOptions::replication(),
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(
            (results[0].id.as_str(), results[0].error.as_deref()),
            ("r", Some("bad_request"))
        );

        // One request per call, without the rejected documents; a call
        // with nothing left to write sends none.
        let only_deep = vec![nested_doc("b", MAX_NESTING_DEPTH + 1)];
        let results = db
            .bulk_docs(only_deep, BulkDocsOptions::new())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].ok);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let ids = |i: usize| -> Vec<String> {
            let body: serde_json::Value =
                rouchdb_core::json::from_slice(&requests[i].body, usize::MAX).unwrap();
            body["docs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["_id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(ids(0), ["a", "c"]);
        assert_eq!(ids(1), ["d"]);
    }

    #[tokio::test]
    async fn put_local_rejects_too_deep_documents() {
        use super::MAX_NESTING_DEPTH;
        let (url, requests) = scripted_server(vec![]).await;
        let deep = nested_doc("x", MAX_NESTING_DEPTH + 1).data;
        let err = adapter_at(&url).put_local("cp", deep).await.unwrap_err();
        assert!(matches!(err, RouchError::BadRequest(_)), "{err:?}");
        assert!(requests.lock().unwrap().is_empty());
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
    async fn all_docs_keys_keeps_error_and_deleted_rows() {
        use rouchdb_core::document::{AllDocsOptions, AllDocsRow};
        // Verbatim CouchDB 3.5.1 reply to `POST _all_docs?include_docs=true`
        // with `{"keys": ["a", "b", "zz"]}` where `b` is deleted: every key
        // gets a row, in order, so `keys[i]` matches `rows[i]`.
        let reply = r#"{"total_rows":2,"offset":null,"rows":[
            {"id":"a","key":"a","value":{"rev":"1-7a7e4b29f3af401e69b6f86e4c26b727"},"doc":{"_id":"a","_rev":"1-7a7e4b29f3af401e69b6f86e4c26b727","v":1}},
            {"id":"b","key":"b","value":{"rev":"2-cc42f3106b98bc7ad82f91bf2382e1df","deleted":true},"doc":null},
            {"key":"zz","error":"not_found"}
        ]}"#;
        let (url, _) = scripted_server(vec![("200 OK", reply.into())]).await;
        let res = adapter_at(&url)
            .all_docs(AllDocsOptions {
                keys: Some(vec!["a".into(), "b".into(), "zz".into()]),
                include_docs: true,
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        let keys: Vec<&str> = res.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["a", "b", "zz"]);
        assert_eq!(
            res.rows[0].rev(),
            Some("1-7a7e4b29f3af401e69b6f86e4c26b727")
        );
        assert_eq!(res.rows[0].doc.as_ref().unwrap()["v"], 1);
        assert!(res.rows[1].is_deleted() && res.rows[1].doc.is_none());
        assert_eq!(res.rows[2], AllDocsRow::not_found("zz"));
        assert_eq!((res.total_rows, res.offset), (2, 0));
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
            matches!(bad, Err(RouchError::InvalidRev(ref r)) if r == "Invalid rev format"),
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
            (
                "401 Unauthorized",
                r#"{"error":"unauthorized","reason":"You are not a server admin."}"#.into(),
            ),
        ])
        .await;
        let db = adapter_at(&url);

        db.destroy().await.unwrap();
        // A database that is already gone counts as destroyed.
        db.destroy().await.unwrap();
        assert!(matches!(db.destroy().await, Err(RouchError::Unauthorized)));
        let lines: Vec<_> = requests
            .lock()
            .unwrap()
            .iter()
            .map(Captured::line)
            .collect();
        assert_eq!(lines, vec!["DELETE /db", "DELETE /db", "DELETE /db"]);
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
            r#"{"error":"bad_request","reason":"Document must be a JSON object"}"#,
        )
        .await;
        assert!(
            matches!(err, RouchError::BadRequest(ref r) if r == "Document must be a JSON object"),
            "{err:?}"
        );
        // A malformed revision is reported like the local adapters do.
        let err = error_for(
            "400 Bad Request",
            r#"{"error":"bad_request","reason":"Invalid rev format"}"#,
        )
        .await;
        assert!(
            matches!(err, RouchError::InvalidRev(ref r) if r == "Invalid rev format"),
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
