use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Result, RouchError};
use crate::rev_tree::RevTree;

// ---------------------------------------------------------------------------
// Revision
// ---------------------------------------------------------------------------

/// A CouchDB revision identifier: `{pos}-{hash}`.
///
/// - `pos` is the generation number (starts at 1, increments each edit).
/// - `hash` is a 32-character hex MD5 digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Revision {
    pub pos: u64,
    pub hash: String,
}

impl Revision {
    pub fn new(pos: u64, hash: String) -> Self {
        Self { pos, hash }
    }

    /// The same revision in CouchDB's canonical form: a 32-digit hex id is
    /// lower case (CouchDB stores it as 16 bytes, so `1-AB…` and `1-ab…`
    /// are the same revision). Other ids are kept as they are.
    pub fn normalized(mut self) -> Self {
        if let std::borrow::Cow::Owned(hash) = normalize_rev_hash(&self.hash) {
            self.hash = hash;
        }
        self
    }
}

/// Canonical form of a revision id (the part after `pos-`): lower case for
/// a 32-digit hex id, unchanged otherwise.
pub fn normalize_rev_hash(hash: &str) -> std::borrow::Cow<'_, str> {
    if hash.len() == 32
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
        && hash.bytes().any(|b| b.is_ascii_uppercase())
    {
        std::borrow::Cow::Owned(hash.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(hash)
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.pos, self.hash)
    }
}

impl FromStr for Revision {
    type Err = RouchError;

    /// Parse `pos-id`. The id is normalized (see [`Revision::normalized`]);
    /// an empty id or one containing NUL (which storage keys cannot hold)
    /// is rejected.
    fn from_str(s: &str) -> Result<Self> {
        let (pos_str, hash) = s
            .split_once('-')
            .ok_or_else(|| RouchError::InvalidRev(s.to_string()))?;
        let pos: u64 = pos_str
            .parse()
            .map_err(|_| RouchError::InvalidRev(s.to_string()))?;
        if hash.is_empty() || hash.contains('\0') {
            return Err(RouchError::InvalidRev(s.to_string()));
        }
        Ok(Revision {
            pos,
            hash: normalize_rev_hash(hash).into_owned(),
        })
    }
}

impl Ord for Revision {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.pos
            .cmp(&other.pos)
            .then_with(|| self.hash.cmp(&other.hash))
    }
}

impl PartialOrd for Revision {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// AttachmentMeta
// ---------------------------------------------------------------------------

/// An attachment of a document revision, as CouchDB describes it in
/// `_attachments`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttachmentMeta {
    pub content_type: String,
    /// Generation of the revision that uploaded the attachment's data
    /// (CouchDB's `revpos`): a write that sends the data (inline, or with
    /// `put_attachment`) sets it to the new revision's generation, even for
    /// bytes identical to the stored ones; stubs and inherited attachments
    /// keep it, and replicated revisions keep the revpos they carry.
    /// `0` when unknown (stored before 0.5, or received without one).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revpos: u64,
    pub digest: String,
    pub length: u64,
    #[serde(default)]
    pub stub: bool,
    /// How the source stores the bytes (CouchDB reports `"gzip"` for
    /// compressed attachments with `att_encoding_info=true`). Only kept
    /// from stubs: rouchdb always stores and serves decoded bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// Size of the encoded bytes at the source (see `encoding`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<u8>>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl AttachmentMeta {
    /// An attachment with inline bytes, as written by `put_attachment` or
    /// an inline `_attachments` member (digest and length computed; the
    /// write sets `revpos`).
    pub fn new(content_type: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            content_type: content_type.into(),
            digest: attachment_digest(&data),
            length: data.len() as u64,
            data: Some(data),
            ..Self::default()
        }
    }

    /// The attachment as a CouchDB `_attachments` member: inline with the
    /// base64 `data` when bytes are given, a stub otherwise. `revpos` is
    /// written when known, `encoding`/`encoded_length` on stubs only (the
    /// inline bytes are always decoded).
    pub fn to_json(&self, data: Option<&[u8]>) -> serde_json::Value {
        use base64::Engine;
        let mut m = serde_json::Map::new();
        m.insert("content_type".into(), self.content_type.clone().into());
        if self.revpos > 0 {
            m.insert("revpos".into(), self.revpos.into());
        }
        m.insert("digest".into(), self.digest.clone().into());
        m.insert("length".into(), self.length.into());
        match data {
            Some(bytes) => {
                m.insert("stub".into(), false.into());
                m.insert(
                    "data".into(),
                    base64::engine::general_purpose::STANDARD
                        .encode(bytes)
                        .into(),
                );
            }
            None => {
                m.insert("stub".into(), true.into());
                if let Some(encoding) = &self.encoding {
                    m.insert("encoding".into(), encoding.clone().into());
                }
                if let Some(len) = self.encoded_length {
                    m.insert("encoded_length".into(), len.into());
                }
            }
        }
        serde_json::Value::Object(m)
    }
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

/// A CouchDB-compatible document.
#[derive(Debug, Clone)]
pub struct Document {
    pub id: String,
    pub rev: Option<Revision>,
    pub deleted: bool,
    pub data: serde_json::Value,
    pub attachments: HashMap<String, AttachmentMeta>,
}

impl Document {
    /// Create a new document from a JSON value.
    ///
    /// Extracts `_id`, `_rev`, `_deleted`, and `_attachments` from the value
    /// and puts the remaining fields in `data`. Other underscore members
    /// (`_revisions`, `_conflicts`, ...) are left in `data` so read paths can
    /// surface them; [`Document::prepare_for_write`] strips or rejects them
    /// before a write.
    ///
    /// Attachments may be stubs (`{"stub": true, "digest": ...}`) or inline
    /// (`{"content_type": ..., "data": "<base64>"}`); `digest` and `length`
    /// are optional for inline data and computed from the decoded bytes.
    pub fn from_json(mut value: serde_json::Value) -> Result<Self> {
        let obj = value
            .as_object_mut()
            .ok_or_else(|| RouchError::BadRequest("document must be a JSON object".into()))?;

        let id = match obj.remove("_id") {
            None => String::new(),
            Some(v) => parse_doc_id(v)?,
        };

        let rev = obj.remove("_rev").map(parse_rev_value).transpose()?;

        let deleted = match obj.remove("_deleted") {
            None => false,
            Some(v) => parse_deleted_value(v)?,
        };

        let attachments = match obj.remove("_attachments") {
            None => HashMap::new(),
            Some(v) => parse_attachments(&v)?,
        };

        Ok(Document {
            id,
            rev,
            deleted,
            data: value,
            attachments,
        })
    }

    /// Normalize a document before a `new_edits=true` write, the way CouchDB
    /// treats special members:
    ///
    /// - the body must be a JSON object;
    /// - `_id`, `_rev`, `_deleted` and `_attachments` left in `data` are
    ///   interpreted (explicit `Document` fields win when both are set);
    /// - read-only metadata (`_conflicts`, `_deleted_conflicts`, `_revs_info`,
    ///   `_revisions`, `_local_seq`) is dropped;
    /// - any other underscore member is rejected, as are ids that start with
    ///   `_` other than `_design/` and `_local/`;
    /// - the revision is normalized ([`Revision::normalized`]);
    /// - a body nested deeper than [`crate::json::MAX_NESTING_DEPTH`] is
    ///   rejected.
    pub fn prepare_for_write(&mut self) -> Result<()> {
        let obj = self
            .data
            .as_object_mut()
            .ok_or_else(|| RouchError::BadRequest("Document must be a JSON object".into()))?;

        let special: Vec<String> = obj.keys().filter(|k| k.starts_with('_')).cloned().collect();
        for key in special {
            let value = obj.remove(&key).unwrap_or_default();
            match key.as_str() {
                "_id" => {
                    let id = parse_doc_id(value)?;
                    if self.id.is_empty() {
                        self.id = id;
                    }
                }
                "_rev" => {
                    let rev = parse_rev_value(value)?;
                    if self.rev.is_none() {
                        self.rev = Some(rev);
                    }
                }
                "_deleted" => {
                    if parse_deleted_value(value)? {
                        self.deleted = true;
                    }
                }
                "_attachments" => {
                    let atts = parse_attachments(&value)?;
                    if self.attachments.is_empty() {
                        self.attachments = atts;
                    }
                }
                k if METADATA_MEMBERS.contains(&k) => {}
                other => {
                    return Err(RouchError::BadRequest(format!(
                        "Bad special document member: {}",
                        other
                    )));
                }
            }
        }

        if self.id.starts_with('_')
            && !self.id.starts_with("_design/")
            && !self.id.starts_with("_local/")
        {
            return Err(RouchError::BadRequest(
                "Only reserved document ids may start with underscore.".into(),
            ));
        }
        if let Some(rev) = self.rev.take() {
            self.rev = Some(rev.normalized());
        }
        crate::json::check_document_depth(&self.data)?;

        Ok(())
    }

    /// Drop read-only metadata members (`_conflicts`, `_revisions`, ...) from
    /// the body without validating anything else. Used for replicated writes,
    /// which must accept whatever the source stored.
    pub fn strip_metadata_members(&mut self) {
        if let Some(obj) = self.data.as_object_mut() {
            for key in METADATA_MEMBERS {
                obj.remove(key);
            }
        }
    }

    /// Convert back to a JSON value with CouchDB underscore fields.
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = match &self.data {
            serde_json::Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };

        obj.insert("_id".into(), serde_json::Value::String(self.id.clone()));

        if let Some(rev) = &self.rev {
            obj.insert("_rev".into(), serde_json::Value::String(rev.to_string()));
        }

        if self.deleted {
            obj.insert("_deleted".into(), serde_json::Value::Bool(true));
        }

        if !self.attachments.is_empty() {
            let att_map = self
                .attachments
                .iter()
                .map(|(name, att)| (name.clone(), att.to_json(att.data.as_deref())))
                .collect();
            obj.insert("_attachments".into(), serde_json::Value::Object(att_map));
        }

        serde_json::Value::Object(obj)
    }
}

/// Underscore members CouchDB accepts on write but never stores in the body.
const METADATA_MEMBERS: [&str; 5] = [
    "_conflicts",
    "_deleted_conflicts",
    "_revs_info",
    "_revisions",
    "_local_seq",
];

fn parse_doc_id(value: serde_json::Value) -> Result<String> {
    match value {
        serde_json::Value::String(s) if s.is_empty() => Err(RouchError::BadRequest(
            "Document id must not be empty".into(),
        )),
        serde_json::Value::String(s) => Ok(s),
        _ => Err(RouchError::BadRequest(
            "Document id must be a string".into(),
        )),
    }
}

fn parse_rev_value(value: serde_json::Value) -> Result<Revision> {
    match value {
        serde_json::Value::String(s) => s.parse(),
        _ => Err(RouchError::BadRequest("Invalid rev format".into())),
    }
}

fn parse_deleted_value(value: serde_json::Value) -> Result<bool> {
    value
        .as_bool()
        .ok_or_else(|| RouchError::BadRequest("Bad special document member: _deleted".into()))
}

/// Parse a CouchDB `_attachments` object.
fn parse_attachments(value: &serde_json::Value) -> Result<HashMap<String, AttachmentMeta>> {
    let obj = value
        .as_object()
        .ok_or_else(|| RouchError::BadRequest("_attachments must be a JSON object".into()))?;
    let mut attachments = HashMap::with_capacity(obj.len());
    for (name, meta) in obj {
        attachments.insert(name.clone(), parse_attachment(name, meta)?);
    }
    Ok(attachments)
}

/// Parse one attachment entry: inline (`data` as base64, with `digest` and
/// `length` optional) or a stub (`stub: true` or a bare `digest`).
fn parse_attachment(name: &str, meta: &serde_json::Value) -> Result<AttachmentMeta> {
    let invalid =
        |why: &str| RouchError::BadRequest(format!("Invalid attachment {}: {}", name, why));
    let obj = meta
        .as_object()
        .ok_or_else(|| invalid("must be a JSON object"))?;

    let content_type = match obj.get("content_type") {
        None | Some(serde_json::Value::Null) => "application/octet-stream".to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(_) => return Err(invalid("content_type must be a string")),
    };

    let revpos = obj.get("revpos").and_then(|v| v.as_u64()).unwrap_or(0);
    if let Some(data) = obj.get("data") {
        use base64::Engine;
        let encoded = data
            .as_str()
            .ok_or_else(|| invalid("data must be a base64 string"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| invalid("data is not valid base64"))?;
        return Ok(AttachmentMeta {
            revpos,
            ..AttachmentMeta::new(content_type, bytes)
        });
    }

    if obj.get("follows").and_then(|v| v.as_bool()) == Some(true) {
        return Err(invalid("multipart attachments (follows) are not supported"));
    }

    // A stub refers to the parent revision's attachment of the same name
    // (CouchDB matches stubs by name), so its digest is optional.
    let is_stub = obj.get("stub").and_then(|v| v.as_bool()).unwrap_or(false);
    let digest = match obj.get("digest").and_then(|v| v.as_str()) {
        Some(digest) => digest.to_string(),
        None if is_stub => String::new(),
        None => return Err(invalid("neither data nor a stub")),
    };
    Ok(AttachmentMeta {
        content_type,
        revpos,
        digest,
        length: obj.get("length").and_then(|v| v.as_u64()).unwrap_or(0),
        stub: true,
        encoding: obj
            .get("encoding")
            .and_then(|v| v.as_str())
            .map(String::from),
        encoded_length: obj.get("encoded_length").and_then(|v| v.as_u64()),
        data: None,
    })
}

/// CouchDB attachment digest: `md5-` followed by the base64 MD5 of the bytes.
pub fn attachment_digest(data: &[u8]) -> String {
    use base64::Engine;
    use md5::{Digest, Md5};
    let hash = Md5::digest(data);
    format!(
        "md5-{}",
        base64::engine::general_purpose::STANDARD.encode(hash)
    )
}

/// Generate the hash part of a new revision id.
///
/// The hash covers the parent revision, the deleted flag, the body and the
/// final attachment set (name, digest, content type), so two replicas that
/// make different edits from the same parent (including attachment-only
/// edits) never produce the same revision id. Documents without attachments
/// hash exactly as before attachments were included.
pub fn generate_rev_hash(
    doc_data: &serde_json::Value,
    deleted: bool,
    prev_rev: Option<&str>,
    attachments: &HashMap<String, AttachmentMeta>,
) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    if let Some(prev) = prev_rev {
        hasher.update(prev.as_bytes());
    }
    hasher.update(if deleted { b"1" } else { b"0" });
    let serialized = serde_json::to_string(doc_data).unwrap_or_default();
    hasher.update(serialized.as_bytes());
    if !attachments.is_empty() {
        let mut names: Vec<&String> = attachments.keys().collect();
        names.sort();
        for name in names {
            let att = &attachments[name];
            hasher.update(b"\0");
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
            hasher.update(att.digest.as_bytes());
            hasher.update(b"\0");
            hasher.update(att.content_type.as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// DocumentMetadata — stored in the database alongside the rev tree
// ---------------------------------------------------------------------------

/// Internal metadata stored per document in the adapter.
#[derive(Debug, Clone)]
pub struct DocMetadata {
    pub id: String,
    pub rev_tree: RevTree,
    pub seq: u64,
}

// ---------------------------------------------------------------------------
// Option / response types shared across the crate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GetOptions {
    /// Retrieve a specific revision.
    pub rev: Option<String>,
    /// Include conflicting revisions in `_conflicts`.
    pub conflicts: bool,
    /// Return all open (leaf) revisions.
    pub open_revs: Option<OpenRevs>,
    /// Include full revision history.
    pub revs: bool,
    /// Include full revision info with status (available/missing/deleted).
    pub revs_info: bool,
    /// If rev is specified and not a leaf, return the latest leaf instead.
    pub latest: bool,
    /// Include inline Base64 attachment data.
    pub attachments: bool,
}

/// Revision info entry returned when `revs_info` is requested.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevInfo {
    pub rev: String,
    pub status: String, // "available", "missing", "deleted"
}

#[derive(Debug, Clone)]
pub enum OpenRevs {
    All,
    Specific(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutResponse {
    pub ok: bool,
    pub id: String,
    pub rev: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocResult {
    pub ok: bool,
    pub id: String,
    pub rev: Option<String>,
    pub error: Option<String>,
    pub reason: Option<String>,
}

/// Options of [`Adapter::bulk_docs`](crate::adapter::Adapter::bulk_docs).
///
/// `BulkDocsOptions::default()` is the same as [`BulkDocsOptions::new`]
/// (normal writes); replication mode must be asked for explicitly with
/// [`BulkDocsOptions::replication`].
#[derive(Debug, Clone)]
pub struct BulkDocsOptions {
    /// When false (replication), accept revisions as-is.
    /// When true (default), generate new revisions and check conflicts.
    pub new_edits: bool,
}

impl BulkDocsOptions {
    /// Normal writes (`new_edits: true`).
    pub fn new() -> Self {
        Self { new_edits: true }
    }

    /// Replication writes (`new_edits: false`).
    pub fn replication() -> Self {
        Self { new_edits: false }
    }
}

impl Default for BulkDocsOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Options of [`Adapter::all_docs`](crate::adapter::Adapter::all_docs).
///
/// `AllDocsOptions::default()` is the same as [`AllDocsOptions::new`]: every
/// document, with an inclusive `end_key` (as in CouchDB).
#[derive(Debug, Clone)]
pub struct AllDocsOptions {
    pub start_key: Option<String>,
    pub end_key: Option<String>,
    pub key: Option<String>,
    pub keys: Option<Vec<String>>,
    pub include_docs: bool,
    pub descending: bool,
    pub skip: u64,
    pub limit: Option<u64>,
    /// Include the `end_key` row itself (default `true`).
    pub inclusive_end: bool,
    /// Include `_conflicts` for each document (requires `include_docs`).
    pub conflicts: bool,
    /// Include `update_seq` in the response.
    pub update_seq: bool,
}

impl AllDocsOptions {
    pub fn new() -> Self {
        Self {
            start_key: None,
            end_key: None,
            key: None,
            keys: None,
            include_docs: false,
            descending: false,
            skip: 0,
            limit: None,
            inclusive_end: true,
            conflicts: false,
            update_seq: false,
        }
    }
}

impl Default for AllDocsOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// A row of an `_all_docs` response.
///
/// A range or `key` query only returns rows for live documents: `id` (equal
/// to `key`) and `value` are set, and `doc` too with `include_docs`.
///
/// A `keys` query returns exactly one row per requested key, in request
/// order (so `rows[i]` answers `keys[i]`), as CouchDB and PouchDB do:
///
/// - a live document: as above;
/// - a deleted document: `value.deleted == Some(true)` and never a `doc`
///   (CouchDB sends `"doc": null` under `include_docs`);
/// - an unknown id: only `key` and `error: Some("not_found")` (see
///   [`AllDocsRow::not_found`]).
///
/// The row is a struct with optional members rather than an enum so that it
/// maps one-to-one onto the CouchDB/PouchDB JSON row and `row.key` is there
/// for every kind of row; [`AllDocsRow::rev`], [`AllDocsRow::is_deleted`]
/// and [`AllDocsRow::is_error`] cover the usual checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllDocsRow {
    /// The document id (`None` for an error row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The requested key; for a document row, its id.
    pub key: String,
    /// The winning revision (`None` for an error row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<AllDocsRowValue>,
    /// The document body, with `include_docs`, for a live document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<serde_json::Value>,
    /// Why there is no document for `key` (`"not_found"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AllDocsRow {
    /// The row of a live or deleted document.
    pub fn document(id: impl Into<String>, value: AllDocsRowValue) -> Self {
        let id = id.into();
        Self {
            key: id.clone(),
            id: Some(id),
            value: Some(value),
            doc: None,
            error: None,
        }
    }

    /// The row of a requested key that names no document:
    /// `{"key": key, "error": "not_found"}`.
    pub fn not_found(key: impl Into<String>) -> Self {
        Self {
            id: None,
            key: key.into(),
            value: None,
            doc: None,
            error: Some("not_found".into()),
        }
    }

    /// The winning revision, unless this is an error row.
    pub fn rev(&self) -> Option<&str> {
        self.value.as_ref().map(|v| v.rev.as_str())
    }

    /// Whether the row is a deleted document (only in `keys` queries).
    pub fn is_deleted(&self) -> bool {
        self.value.as_ref().and_then(|v| v.deleted) == Some(true)
    }

    /// Whether the row is an error row (a key with no document).
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

/// The `value` of an [`AllDocsRow`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllDocsRowValue {
    pub rev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllDocsResponse {
    pub total_rows: u64,
    /// The memory and redb adapters report the `skip` that was applied, as
    /// PouchDB's local adapters do. CouchDB (and so the http adapter)
    /// reports the number of rows before the first returned one, including
    /// those before `start_key`, which needs a counted index the local
    /// stores do not keep.
    pub offset: u64,
    pub rows: Vec<AllDocsRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_seq: Option<Seq>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbInfo {
    pub db_name: String,
    pub doc_count: u64,
    #[serde(default)]
    pub doc_del_count: u64,
    pub update_seq: Seq,
}

// ---------------------------------------------------------------------------
// Changes types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ChangesOptions {
    pub since: Seq,
    pub limit: Option<u64>,
    pub descending: bool,
    pub include_docs: bool,
    pub live: bool,
    pub doc_ids: Option<Vec<String>>,
    pub selector: Option<serde_json::Value>,
    /// Include conflicting revisions per change event.
    pub conflicts: bool,
    /// Changes style: `MainOnly` (default) returns only winning rev,
    /// `AllDocs` returns all leaf revisions.
    pub style: ChangesStyle,
}

/// Controls which revisions appear in each change event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ChangesStyle {
    /// Default: only the winning revision.
    #[default]
    MainOnly,
    /// All leaf revisions (including deleted ones), matching CouchDB.
    AllDocs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEvent {
    pub seq: Seq,
    pub id: String,
    pub changes: Vec<ChangeRev>,
    /// Omitted from the JSON when false, as CouchDB does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<serde_json::Value>,
    /// Conflicting revisions (when `conflicts: true` requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRev {
    pub rev: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangesResponse {
    pub results: Vec<ChangeEvent>,
    pub last_seq: Seq,
}

// ---------------------------------------------------------------------------
// Replication-related types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkGetItem {
    pub id: String,
    pub rev: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkGetResponse {
    pub results: Vec<BulkGetResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkGetResult {
    pub id: String,
    pub docs: Vec<BulkGetDoc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkGetDoc {
    pub ok: Option<serde_json::Value>,
    pub error: Option<BulkGetError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkGetError {
    pub id: String,
    pub rev: String,
    pub error: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevsDiffResponse {
    #[serde(flatten)]
    pub results: HashMap<String, RevsDiffResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevsDiffResult {
    pub missing: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub possible_ancestors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Sequence type — supports both numeric (local) and opaque string (CouchDB)
// ---------------------------------------------------------------------------

/// A database sequence identifier.
///
/// Local adapters use numeric sequences (0, 1, 2, ...).
/// CouchDB 3.x uses opaque string sequences that must be passed back as-is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Seq {
    Num(u64),
    Str(String),
}

impl Seq {
    /// The zero sequence (start from the beginning).
    pub fn zero() -> Self {
        Seq::Num(0)
    }

    /// Extract the numeric value. For opaque strings, parses the numeric
    /// prefix (e.g., `"13-abc..."` → `13`). Returns 0 if unparseable.
    pub fn as_num(&self) -> u64 {
        match self {
            Seq::Num(n) => *n,
            Seq::Str(s) => s
                .split('-')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        }
    }

    /// Format for use in HTTP query parameters.
    pub fn to_query_string(&self) -> String {
        match self {
            Seq::Num(n) => n.to_string(),
            Seq::Str(s) => s.clone(),
        }
    }
}

impl Default for Seq {
    fn default() -> Self {
        Seq::Num(0)
    }
}

impl From<u64> for Seq {
    fn from(n: u64) -> Self {
        Seq::Num(n)
    }
}

impl std::fmt::Display for Seq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Seq::Num(n) => write!(f, "{}", n),
            Seq::Str(s) => write!(f, "{}", s),
        }
    }
}

// ---------------------------------------------------------------------------
// Purge types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgeResponse {
    pub purge_seq: Option<u64>,
    pub purged: HashMap<String, Vec<String>>,
}

// ---------------------------------------------------------------------------
// Security document
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityDocument {
    #[serde(default)]
    pub admins: SecurityGroup,
    #[serde(default)]
    pub members: SecurityGroup,
    /// Arbitrary additional fields CouchDB permits in `_security` are preserved
    /// so they round-trip instead of being silently dropped.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityGroup {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

// ---------------------------------------------------------------------------
// Attachment options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GetAttachmentOptions {
    pub rev: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_display_and_parse() {
        let rev = Revision::new(3, "abc123".into());
        assert_eq!(rev.to_string(), "3-abc123");

        let parsed: Revision = "3-abc123".parse().unwrap();
        assert_eq!(parsed, rev);
    }

    #[test]
    fn revision_ids_are_normalized_like_couchdb() {
        let upper = format!("2-{}", "AB".repeat(16));
        let rev: Revision = upper.parse().unwrap();
        assert_eq!(rev.to_string(), format!("2-{}", "ab".repeat(16)));
        assert_eq!(
            Revision::new(2, "AB".repeat(16)).normalized(),
            Revision::new(2, "ab".repeat(16))
        );
        // Only 32-digit hex ids are canonicalized; others are kept.
        for kept in ["1-ABC", "1-Zz", "0-1"] {
            assert_eq!(kept.parse::<Revision>().unwrap().to_string(), kept);
        }
        let not_hex = format!("1-{}", "Z".repeat(32));
        assert_eq!(not_hex.parse::<Revision>().unwrap().to_string(), not_hex);
        let short = format!("1-{}", "A".repeat(31));
        assert_eq!(short.parse::<Revision>().unwrap().to_string(), short);
        // NUL cannot be stored in a revision id.
        assert!(matches!(
            "1-a\u{0}b".parse::<Revision>(),
            Err(RouchError::InvalidRev(_))
        ));
    }

    #[test]
    fn prepare_for_write_normalizes_and_limits_depth() {
        let mut doc = Document::from_json(serde_json::json!({"v": 1})).unwrap();
        doc.rev = Some(Revision::new(1, "F".repeat(32)));
        doc.prepare_for_write().unwrap();
        assert_eq!(doc.rev.unwrap().hash, "f".repeat(32));

        let mut deep = serde_json::json!(1);
        for _ in 0..crate::json::MAX_NESTING_DEPTH {
            deep = serde_json::json!([deep]);
        }
        let mut doc = Document::from_json(serde_json::json!({ "v": deep })).unwrap();
        assert!(matches!(
            doc.prepare_for_write(),
            Err(RouchError::BadRequest(ref r)) if r.contains("nesting")
        ));
    }

    #[test]
    fn stub_without_digest_is_a_stub() {
        let doc = Document::from_json(serde_json::json!({
            "_attachments": {"a.txt": {"stub": true, "length": 5}}
        }))
        .unwrap();
        let att = &doc.attachments["a.txt"];
        assert!(att.stub && att.data.is_none() && att.digest.is_empty());
        assert_eq!(att.length, 5);
        assert!(Document::from_json(serde_json::json!({"_attachments": {"a": {}}})).is_err());
    }

    #[test]
    fn revision_ordering() {
        let r1 = Revision::new(1, "aaa".into());
        let r2 = Revision::new(2, "aaa".into());
        let r3 = Revision::new(2, "bbb".into());
        assert!(r1 < r2);
        assert!(r2 < r3);
    }

    #[test]
    fn invalid_revision() {
        assert!("nope".parse::<Revision>().is_err());
        assert!("abc-123".parse::<Revision>().is_err());
    }

    #[test]
    fn revision_rejects_empty_hash() {
        // "{pos}-" with no hash is malformed and must be rejected.
        assert!("3-".parse::<Revision>().is_err());
        assert!("1-".parse::<Revision>().is_err());
    }

    #[test]
    fn to_json_inline_attachment_is_base64() {
        let mut attachments = HashMap::new();
        attachments.insert(
            "hi.txt".into(),
            AttachmentMeta {
                content_type: "text/plain".into(),
                digest: "md5-abc".into(),
                length: 3,
                stub: false,
                data: Some(b"hi!".to_vec()),
                ..Default::default()
            },
        );
        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({}),
            attachments,
        };
        let json = doc.to_json();
        // CouchDB requires inline data as a base64 string, not a byte array.
        assert_eq!(json["_attachments"]["hi.txt"]["data"], "aGkh");
        assert_eq!(json["_attachments"]["hi.txt"]["stub"], false);
    }

    #[test]
    fn attachment_revpos_and_encoding_round_trip() {
        // A stub as CouchDB 3.5.1 lists it (`GET /db/e?att_encoding_info=true`).
        let stub = serde_json::json!({"content_type": "text/plain", "revpos": 1,
            "digest": "md5-Ew9RIaBldynHDFVo1PvkrA==", "length": 2400, "stub": true,
            "encoding": "gzip", "encoded_length": 52});
        let doc = Document::from_json(serde_json::json!({
            "_id": "e", "_rev": "1-3b5073b1b7a6ec2abcd4b0d8e005da08",
            "_attachments": {"big.txt": stub}
        }))
        .unwrap();
        let meta = &doc.attachments["big.txt"];
        assert_eq!(meta.revpos, 1);
        assert_eq!(meta.encoding.as_deref(), Some("gzip"));
        assert_eq!(meta.encoded_length, Some(52));
        assert_eq!(doc.to_json()["_attachments"]["big.txt"], stub);

        // Inline data is decoded bytes: its revpos is kept, an encoding is
        // not (CouchDB ignores it too).
        let doc = Document::from_json(serde_json::json!({
            "_id": "d",
            "_attachments": {"hi.txt": {"content_type": "text/plain", "revpos": 3,
                "digest": "md5-O9yO4zjoapsrEQwYrCDNZw==", "data": "aGkh", "encoding": "gzip"}}
        }))
        .unwrap();
        let meta = &doc.attachments["hi.txt"];
        assert_eq!((meta.revpos, meta.encoding.as_deref()), (3, None));
        // (The digest is recomputed from the bytes: CouchDB's is the MD5 of
        // the gzip-compressed bytes it stores for text types.)
        assert_eq!(
            doc.to_json()["_attachments"]["hi.txt"],
            serde_json::json!({"content_type": "text/plain", "revpos": 3,
                "digest": attachment_digest(b"hi!"), "length": 3, "stub": false,
                "data": "aGkh"})
        );

        // Without a revpos (unknown), none is written.
        let doc = Document::from_json(serde_json::json!({
            "_id": "d", "_attachments": {"x": {"stub": true, "digest": "md5-x", "length": 1}}
        }))
        .unwrap();
        assert_eq!(doc.attachments["x"].revpos, 0);
        assert!(doc.to_json()["_attachments"]["x"].get("revpos").is_none());
        assert_eq!(
            AttachmentMeta::new("text/plain", b"hi!".to_vec()).digest,
            attachment_digest(b"hi!")
        );
        // The serde form omits an unknown revpos too.
        let meta = |revpos| AttachmentMeta {
            revpos,
            ..AttachmentMeta::default()
        };
        assert!(
            serde_json::to_value(meta(0))
                .unwrap()
                .get("revpos")
                .is_none()
        );
        assert_eq!(serde_json::to_value(meta(3)).unwrap()["revpos"], 3);
    }

    #[test]
    fn document_from_json_roundtrip() {
        let json = serde_json::json!({
            "_id": "doc1",
            "_rev": "1-abc",
            "name": "Alice",
            "age": 30
        });

        let doc = Document::from_json(json).unwrap();
        assert_eq!(doc.id, "doc1");
        assert_eq!(doc.rev.as_ref().unwrap().to_string(), "1-abc");
        assert_eq!(doc.data["name"], "Alice");
        assert!(!doc.data.as_object().unwrap().contains_key("_id"));

        let back = doc.to_json();
        assert_eq!(back["_id"], "doc1");
        assert_eq!(back["_rev"], "1-abc");
        assert_eq!(back["name"], "Alice");
    }

    #[test]
    fn document_from_json_minimal() {
        let json = serde_json::json!({"hello": "world"});
        let doc = Document::from_json(json).unwrap();
        assert!(doc.id.is_empty());
        assert!(doc.rev.is_none());
        assert!(!doc.deleted);
    }

    #[test]
    fn bulk_docs_options_defaults() {
        let opts = BulkDocsOptions::new();
        assert!(opts.new_edits);

        let repl = BulkDocsOptions::replication();
        assert!(!repl.new_edits);
    }

    #[test]
    fn all_docs_rows_match_couchdb_json() {
        // The three kinds of rows of a CouchDB 3.5.1 `keys` reply.
        let live: AllDocsRow = serde_json::from_value(
            serde_json::json!({"id": "a", "key": "a", "value": {"rev": "1-x"}}),
        )
        .unwrap();
        let gone: AllDocsRow = serde_json::from_value(serde_json::json!(
            {"id": "b", "key": "b", "value": {"rev": "2-y", "deleted": true}, "doc": null}
        ))
        .unwrap();
        let missing: AllDocsRow =
            serde_json::from_value(serde_json::json!({"key": "zz", "error": "not_found"})).unwrap();
        assert_eq!(
            (live.rev(), live.is_deleted(), live.is_error()),
            (Some("1-x"), false, false)
        );
        assert_eq!(
            (gone.rev(), gone.is_deleted(), gone.is_error()),
            (Some("2-y"), true, false)
        );
        assert_eq!(
            (missing.rev(), missing.is_deleted(), missing.is_error()),
            (None, false, true)
        );
        assert_eq!(missing, AllDocsRow::not_found("zz"));
        assert_eq!(
            live,
            AllDocsRow::document(
                "a",
                AllDocsRowValue {
                    rev: "1-x".into(),
                    deleted: None
                }
            )
        );
        // Serialized back without the members a row does not have.
        assert_eq!(
            serde_json::to_value(&missing).unwrap(),
            serde_json::json!({"key": "zz", "error": "not_found"})
        );
        assert_eq!(
            serde_json::to_value(&gone).unwrap(),
            serde_json::json!({"id": "b", "key": "b", "value": {"rev": "2-y", "deleted": true}})
        );
    }

    #[test]
    fn option_defaults_match_new() {
        // F23: `Default` must not silently switch to replication mode or to
        // an exclusive end key; it is the same as `new()`.
        assert!(BulkDocsOptions::default().new_edits);
        let all_docs = AllDocsOptions::default();
        assert!(all_docs.inclusive_end);
        assert_eq!(
            format!("{:?}", all_docs),
            format!("{:?}", AllDocsOptions::new())
        );
    }

    #[test]
    fn to_json_deleted_document() {
        let doc = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(2, "def".into())),
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        let json = doc.to_json();
        assert_eq!(json["_deleted"], true);
        assert_eq!(json["_id"], "doc1");
        assert_eq!(json["_rev"], "2-def");
    }

    #[test]
    fn to_json_with_attachments() {
        let mut attachments = HashMap::new();
        attachments.insert(
            "file.txt".into(),
            AttachmentMeta {
                content_type: "text/plain".into(),
                digest: "md5-abc".into(),
                length: 100,
                stub: true,
                data: None,
                ..Default::default()
            },
        );
        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"key": "val"}),
            attachments,
        };
        let json = doc.to_json();
        assert!(json["_attachments"]["file.txt"].is_object());
        assert_eq!(
            json["_attachments"]["file.txt"]["content_type"],
            "text/plain"
        );
    }

    #[test]
    fn to_json_non_object_data() {
        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!("just a string"),
            attachments: HashMap::new(),
        };
        let json = doc.to_json();
        assert_eq!(json["_id"], "doc1");
    }

    #[test]
    fn document_from_json_with_deleted_and_attachments() {
        let json = serde_json::json!({
            "_id": "doc1",
            "_rev": "1-abc",
            "_deleted": true,
            "_attachments": {
                "photo.jpg": {
                    "content_type": "image/jpeg",
                    "digest": "md5-xyz",
                    "length": 500,
                    "stub": true
                }
            },
            "name": "test"
        });
        let doc = Document::from_json(json).unwrap();
        assert!(doc.deleted);
        assert_eq!(doc.attachments.len(), 1);
        assert_eq!(doc.attachments["photo.jpg"].content_type, "image/jpeg");
    }

    // --- F03: CouchDB/PouchDB inline attachments ---

    #[test]
    fn from_json_minimal_inline_attachment() {
        // The minimal inline form PouchDB and CouchDB accept: no digest/length.
        let doc = Document::from_json(serde_json::json!({
            "_id": "d",
            "_attachments": {"hi.txt": {"content_type": "text/plain", "data": "aGkh"}}
        }))
        .unwrap();
        let att = &doc.attachments["hi.txt"];
        assert_eq!(att.data.as_deref(), Some(&b"hi!"[..]));
        assert_eq!(att.length, 3);
        assert!(!att.stub);
        assert_eq!(att.digest, attachment_digest(b"hi!"));
        assert_eq!(att.content_type, "text/plain");
    }

    #[test]
    fn from_json_couchdb_inline_attachment_without_length() {
        // GET ?attachments=true from CouchDB omits `length` for inline data.
        let doc = Document::from_json(serde_json::json!({
            "_id": "d",
            "_attachments": {"hi.txt": {
                "content_type": "text/plain",
                "revpos": 1,
                "digest": "md5-3Qbt4MdZDzgyIUxhEaH08A==",
                "data": "aGkh"
            }}
        }))
        .unwrap();
        assert_eq!(doc.attachments["hi.txt"].length, 3);
        assert_eq!(doc.attachments["hi.txt"].data.as_deref(), Some(&b"hi!"[..]));
    }

    #[test]
    fn from_json_attachment_defaults_content_type() {
        let doc = Document::from_json(serde_json::json!({
            "_attachments": {"blob": {"data": "aGkh"}}
        }))
        .unwrap();
        assert_eq!(
            doc.attachments["blob"].content_type,
            "application/octet-stream"
        );
    }

    #[test]
    fn from_json_attachment_stub_without_data() {
        let doc = Document::from_json(serde_json::json!({
            "_attachments": {"a.txt": {
                "content_type": "text/plain", "revpos": 2, "digest": "md5-x",
                "length": 7, "stub": true
            }}
        }))
        .unwrap();
        let att = &doc.attachments["a.txt"];
        assert!(att.stub);
        assert!(att.data.is_none());
        assert_eq!(att.digest, "md5-x");
        assert_eq!(att.length, 7);
    }

    #[test]
    fn from_json_rejects_invalid_attachments() {
        // Invalid base64.
        assert!(
            Document::from_json(serde_json::json!({
                "_attachments": {"a": {"content_type": "text/plain", "data": "!!!"}}
            }))
            .is_err()
        );
        // Neither data nor a stub.
        assert!(
            Document::from_json(serde_json::json!({
                "_attachments": {"a": {"content_type": "text/plain"}}
            }))
            .is_err()
        );
        // Not an object.
        assert!(Document::from_json(serde_json::json!({"_attachments": 5})).is_err());
        assert!(Document::from_json(serde_json::json!({"_attachments": {"a": 5}})).is_err());
    }

    // --- F24: wrongly typed special fields ---

    #[test]
    fn from_json_rejects_wrongly_typed_special_fields() {
        assert!(Document::from_json(serde_json::json!({"_id": 42})).is_err());
        assert!(Document::from_json(serde_json::json!({"_id": ""})).is_err());
        assert!(Document::from_json(serde_json::json!({"_rev": 5})).is_err());
        assert!(Document::from_json(serde_json::json!({"_deleted": "true"})).is_err());
        // Well-typed values still work.
        let doc = Document::from_json(serde_json::json!({"_id": "x", "_deleted": false})).unwrap();
        assert_eq!(doc.id, "x");
        assert!(!doc.deleted);
    }

    // --- F07 / F64: normalization before a new_edits write ---

    fn doc_with(data: serde_json::Value) -> Document {
        Document {
            id: "d".into(),
            rev: None,
            deleted: false,
            data,
            attachments: HashMap::new(),
        }
    }

    #[test]
    fn prepare_for_write_interprets_special_members() {
        let mut doc = doc_with(serde_json::json!({
            "_deleted": true,
            "_rev": "1-abc",
            "_attachments": {"a.txt": {"content_type": "text/plain", "data": "aGkh"}},
            "_conflicts": ["2-x"],
            "_deleted_conflicts": ["2-y"],
            "_revs_info": [],
            "_revisions": {"start": 1, "ids": ["abc"]},
            "_local_seq": 3,
            "x": 1
        }));
        doc.prepare_for_write().unwrap();
        assert!(doc.deleted);
        assert_eq!(doc.rev.as_ref().unwrap().to_string(), "1-abc");
        assert_eq!(doc.attachments["a.txt"].length, 3);
        assert_eq!(doc.data, serde_json::json!({"x": 1}));
    }

    #[test]
    fn prepare_for_write_keeps_explicit_fields() {
        // Explicit Document fields win over leftovers in the body.
        let mut doc = doc_with(serde_json::json!({"_id": "other", "_rev": "9-zzz", "x": 1}));
        doc.rev = Some("2-abc".parse().unwrap());
        doc.prepare_for_write().unwrap();
        assert_eq!(doc.id, "d");
        assert_eq!(doc.rev.as_ref().unwrap().to_string(), "2-abc");
        assert_eq!(doc.data, serde_json::json!({"x": 1}));
    }

    #[test]
    fn prepare_for_write_rejects_unknown_special_member() {
        let mut doc = doc_with(serde_json::json!({"_foo": 1}));
        let err = doc.prepare_for_write().unwrap_err();
        assert!(err.to_string().contains("_foo"), "{}", err);
    }

    #[test]
    fn prepare_for_write_rejects_non_object_body() {
        let mut doc = doc_with(serde_json::json!([1, 2, 3]));
        assert!(doc.prepare_for_write().is_err());
        let mut doc = doc_with(serde_json::json!("text"));
        assert!(doc.prepare_for_write().is_err());
    }

    #[test]
    fn prepare_for_write_validates_reserved_ids() {
        let mut doc = doc_with(serde_json::json!({}));
        doc.id = "_bad".into();
        assert!(doc.prepare_for_write().is_err());
        let mut doc = doc_with(serde_json::json!({}));
        doc.id = "_design/app".into();
        assert!(doc.prepare_for_write().is_ok());
    }

    // --- F06: the rev hash covers attachments ---

    fn att_meta(digest: &str, content_type: &str) -> AttachmentMeta {
        AttachmentMeta {
            content_type: content_type.into(),
            digest: digest.into(),
            length: 3,
            stub: true,
            data: None,
            ..Default::default()
        }
    }

    #[test]
    fn rev_hash_depends_on_attachments() {
        let body = serde_json::json!({"a": 1});
        let none = generate_rev_hash(&body, false, Some("1-x"), &HashMap::new());
        let a: HashMap<_, _> = [("f".to_string(), att_meta("md5-AAA", "text/plain"))].into();
        let b: HashMap<_, _> = [("f".to_string(), att_meta("md5-BBB", "text/plain"))].into();
        let ha = generate_rev_hash(&body, false, Some("1-x"), &a);
        let hb = generate_rev_hash(&body, false, Some("1-x"), &b);
        assert_ne!(ha, hb);
        assert_ne!(ha, none);
        // Deterministic.
        assert_eq!(ha, generate_rev_hash(&body, false, Some("1-x"), &a));
        // The attachment name and the content type are covered too.
        let renamed: HashMap<_, _> = [("g".to_string(), att_meta("md5-AAA", "text/plain"))].into();
        assert_ne!(ha, generate_rev_hash(&body, false, Some("1-x"), &renamed));
        let retyped: HashMap<_, _> = [("f".to_string(), att_meta("md5-AAA", "image/png"))].into();
        assert_ne!(ha, generate_rev_hash(&body, false, Some("1-x"), &retyped));
    }

    #[test]
    fn rev_hash_depends_on_deleted_and_parent() {
        // A tombstone and an empty-body edit of the same parent must not
        // share a revision id, and neither may edits of different parents.
        let empty = serde_json::json!({});
        let live = generate_rev_hash(&empty, false, Some("1-abc"), &HashMap::new());
        let tomb = generate_rev_hash(&empty, true, Some("1-abc"), &HashMap::new());
        assert_ne!(live, tomb);
        assert_ne!(
            live,
            generate_rev_hash(&empty, false, Some("1-abd"), &HashMap::new())
        );
        assert_ne!(
            live,
            generate_rev_hash(&empty, false, None, &HashMap::new())
        );
    }

    #[test]
    fn rev_hash_golden_values() {
        // Revision ids are persisted and exchanged with other replicas, so the
        // algorithm must never change silently: md5 over parent rev, deleted
        // flag, JSON body and, only when present, the sorted attachment set
        // (`\0name\0digest\0content_type` per attachment). Documents without
        // attachments keep the historical hash.
        let alice = serde_json::json!({"name": "Alice"});
        let none = HashMap::new();
        assert_eq!(
            generate_rev_hash(&alice, false, Some("1-abc"), &none),
            "f133ae7ee0488fb8b317cdbde22abd5a"
        );
        assert_eq!(
            generate_rev_hash(&alice, false, None, &none),
            "20225af052e2e499c2693fe0977044fa"
        );
        let empty = serde_json::json!({});
        assert_eq!(
            generate_rev_hash(&empty, false, Some("1-abc"), &none),
            "b4ad2a06747d1afbfa67965f67605fd5"
        );
        assert_eq!(
            generate_rev_hash(&empty, true, Some("1-abc"), &none),
            "6775d54c2efb18e61bb66a15b74da837"
        );
        let one: HashMap<_, _> = [("f.txt".to_string(), att_meta("md5-AAA", "text/plain"))].into();
        assert_eq!(
            generate_rev_hash(&alice, false, Some("1-abc"), &one),
            "38b695b617e8cf91c097faa5096f18ec"
        );
        // Attachments are hashed in name order, whatever the map order.
        let two: HashMap<_, _> = [
            ("b".to_string(), att_meta("md5-B", "image/png")),
            ("a".to_string(), att_meta("md5-A", "text/plain")),
        ]
        .into();
        assert_eq!(
            generate_rev_hash(&alice, false, Some("1-abc"), &two),
            "a6039fa3778c7444a1870165af951a01"
        );
    }

    #[test]
    fn document_from_json_not_object() {
        let json = serde_json::json!("just a string");
        assert!(Document::from_json(json).is_err());
    }

    #[test]
    fn seq_str_as_num() {
        let seq = Seq::Str("42-g1AAAABXeJzLY".into());
        assert_eq!(seq.as_num(), 42);

        let seq2 = Seq::Str("not-a-number".into());
        assert_eq!(seq2.as_num(), 0);
    }

    #[test]
    fn seq_to_query_string() {
        assert_eq!(Seq::Num(5).to_query_string(), "5");
        let opaque = "13-g1AAAABXeJzLY".to_string();
        assert_eq!(Seq::Str(opaque.clone()).to_query_string(), opaque);
    }

    #[test]
    fn seq_display() {
        assert_eq!(format!("{}", Seq::Num(42)), "42");
        assert_eq!(format!("{}", Seq::Str("opaque-seq".into())), "opaque-seq");
    }

    #[test]
    fn seq_from_u64() {
        let seq: Seq = 7u64.into();
        assert_eq!(seq, Seq::Num(7));
    }
}
