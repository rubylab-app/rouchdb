# Core Types Reference

All core types are defined in the `rouchdb-core` crate and re-exported from the top-level `rouchdb` crate. You can import them with:

```rust
use rouchdb::*; // Re-exports all types from rouchdb-core::document
```

---

## Document

The fundamental unit of data in RouchDB.

```rust
pub struct Document {
    pub id: String,
    pub rev: Option<Revision>,
    pub deleted: bool,
    pub data: serde_json::Value,
    pub attachments: HashMap<String, AttachmentMeta>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | `String` | The document's unique identifier (`_id` in CouchDB). |
| `rev` | `Option<Revision>` | The current revision. `None` for new documents that have not yet been written. |
| `deleted` | `bool` | Whether this document is a deletion tombstone (`_deleted` in CouchDB). |
| `data` | `serde_json::Value` | The document body as a JSON value. Does not include underscore-prefixed CouchDB metadata fields (`_id`, `_rev`, `_deleted`, `_attachments`). |
| `attachments` | `HashMap<String, AttachmentMeta>` | Map of attachment names to their metadata. |

### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `from_json` | `fn from_json(value: serde_json::Value) -> Result<Self>` | Parse a CouchDB-style JSON object. Extracts `_id`, `_rev`, `_deleted`, and `_attachments` from the value and puts the remaining fields in `data`. Returns `RouchError::BadRequest` if the value is not a JSON object. |
| `to_json` | `fn to_json(&self) -> serde_json::Value` | Convert back to a CouchDB-style JSON object with underscore-prefixed metadata fields included. |

### Example

```rust
// Parse from CouchDB JSON
let doc = Document::from_json(json!({
    "_id": "user:alice",
    "_rev": "3-abc123",
    "name": "Alice",
    "age": 30
}))?;

assert_eq!(doc.id, "user:alice");
assert_eq!(doc.data["name"], "Alice");

// Convert back
let json = doc.to_json();
assert_eq!(json["_id"], "user:alice");
assert_eq!(json["_rev"], "3-abc123");
```

---

## Revision

A CouchDB revision identifier in the format `{pos}-{hash}`.

```rust
pub struct Revision {
    pub pos: u64,
    pub hash: String,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `pos` | `u64` | The generation number. Starts at 1, increments with each edit. |
| `hash` | `String` | A hex digest string identifying this specific revision. |

### Trait Implementations

| Trait | Behavior |
|-------|----------|
| `Display` | Formats as `"{pos}-{hash}"` (e.g., `"3-abc123"`). |
| `FromStr` | Parses from `"{pos}-{hash}"` format. Returns `RouchError::InvalidRev` on failure. |
| `Ord` / `PartialOrd` | Orders by `pos` first, then by `hash` lexicographically. This is the deterministic winning revision algorithm used by CouchDB. |
| `Eq` / `Hash` | Two revisions are equal if both `pos` and `hash` match. |
| `Serialize` / `Deserialize` | Serializes as a JSON object with `pos` and `hash` fields. |

### Example

```rust
// Create directly
let rev = Revision::new(3, "abc123".into());
assert_eq!(rev.to_string(), "3-abc123");

// Parse from string
let parsed: Revision = "3-abc123".parse()?;
assert_eq!(parsed.pos, 3);
assert_eq!(parsed.hash, "abc123");

// Ordering (deterministic winner)
let r1 = Revision::new(2, "aaa".into());
let r2 = Revision::new(2, "bbb".into());
assert!(r1 < r2); // Same pos, "bbb" > "aaa" lexicographically
```

---

## Seq

A database sequence identifier. Local adapters use numeric sequences; CouchDB 3.x uses opaque string sequences.

```rust
pub enum Seq {
    Num(u64),
    Str(String),
}
```

| Variant | Description |
|---------|-------------|
| `Num(u64)` | Numeric sequence, used by local adapters (memory, redb). Starts at 0. |
| `Str(String)` | Opaque string sequence, used by CouchDB 3.x. Must be passed back as-is. |

### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `zero` | `fn zero() -> Self` | Returns `Seq::Num(0)` -- the starting point (beginning of changes). |
| `as_num` | `fn as_num(&self) -> u64` | Extract the numeric value. For `Str` variants, parses the numeric prefix before the first `-` (e.g., `"13-g1A..."` returns `13`). Returns `0` if unparseable. |
| `to_query_string` | `fn to_query_string(&self) -> String` | Format for use in HTTP query parameters. `Num` becomes its decimal string, `Str` is returned as-is. |

### Trait Implementations

| Trait | Behavior |
|-------|----------|
| `Default` | Returns `Seq::Num(0)`. |
| `Display` | Formats the numeric value or string. |
| `From<u64>` | Creates `Seq::Num(n)`. |
| `Serialize` / `Deserialize` | Uses `#[serde(untagged)]` -- serializes as either a number or string in JSON. |

---

## DbInfo

Database metadata returned by `Adapter::info()`.

```rust
pub struct DbInfo {
    pub db_name: String,
    pub doc_count: u64,
    pub update_seq: Seq,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `db_name` | `String` | The name of the database. |
| `doc_count` | `u64` | Number of non-deleted documents. |
| `update_seq` | `Seq` | The current update sequence number. Increments with every write. |

---

## DocResult

The result of a single document write operation.

```rust
pub struct DocResult {
    pub ok: bool,
    pub id: String,
    pub rev: Option<String>,
    pub error: Option<String>,
    pub reason: Option<String>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `ok` | `bool` | `true` if the write succeeded. |
| `id` | `String` | The document ID. |
| `rev` | `Option<String>` | The new revision string if the write succeeded. |
| `error` | `Option<String>` | Error type (e.g., `"conflict"`) if the write failed. |
| `reason` | `Option<String>` | Human-readable error description. |

---

## AttachmentMeta

Metadata for a document attachment.

```rust
pub struct AttachmentMeta {
    pub content_type: String,
    pub digest: String,
    pub length: u64,
    pub stub: bool,
    pub data: Option<Vec<u8>>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `content_type` | `String` | MIME type (e.g., `"image/png"`). |
| `digest` | `String` | Content digest for deduplication. |
| `length` | `u64` | Size in bytes. |
| `stub` | `bool` | If `true`, only metadata is present (no inline data). Defaults to `false`. |
| `data` | `Option<Vec<u8>>` | Inline binary data, if available. Omitted from serialization when `None`. |

---

## DocMetadata

Internal metadata stored per document in an adapter. Not typically used in application code.

```rust
pub struct DocMetadata {
    pub id: String,
    pub rev_tree: RevTree,
    pub seq: u64,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | `String` | The document ID. |
| `rev_tree` | `RevTree` | The full revision tree for this document. |
| `seq` | `u64` | The last sequence number at which this document was modified. |

---

## PutResponse

Simple response for a successful document write (used in some internal paths).

```rust
pub struct PutResponse {
    pub ok: bool,
    pub id: String,
    pub rev: String,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `ok` | `bool` | Always `true` for success. |
| `id` | `String` | The document ID. |
| `rev` | `String` | The new revision string. |

---

## Options Structs

### GetOptions

Options for document retrieval.

```rust
pub struct GetOptions {
    pub rev: Option<String>,
    pub conflicts: bool,
    pub open_revs: Option<OpenRevs>,
    pub revs: bool,
    pub revs_info: bool,
    pub latest: bool,
    pub attachments: bool,
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `rev` | `Option<String>` | `None` | Retrieve a specific revision instead of the winner. |
| `conflicts` | `bool` | `false` | Include conflicting revisions in the response. |
| `open_revs` | `Option<OpenRevs>` | `None` | Return all open (leaf) revisions. |
| `revs` | `bool` | `false` | Include full revision history. |
| `revs_info` | `bool` | `false` | Include revision info with status (`available`, `missing`, `deleted`) for each revision. |
| `latest` | `bool` | `false` | If `rev` is specified and is not a leaf, return the latest leaf revision instead. |
| `attachments` | `bool` | `false` | Include inline Base64 attachment data in the response. |

#### OpenRevs

```rust
pub enum OpenRevs {
    All,
    Specific(Vec<String>),
}
```

| Variant | Description |
|---------|-------------|
| `All` | Return all leaf revisions. |
| `Specific(Vec<String>)` | Return only these specific revisions. |

---

### BulkDocsOptions

Options for bulk document writes.

```rust
pub struct BulkDocsOptions {
    pub new_edits: bool,
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `new_edits` | `bool` | `true` (via `BulkDocsOptions::new()`) | When `true`, the adapter generates new revisions and checks for conflicts. When `false` (replication mode), revisions are accepted as-is and merged into the revision tree. |

**Note:** The `Default` trait implementation sets `new_edits` to `false`. Use `BulkDocsOptions::new()` for user-mode writes (which sets `new_edits: true`) and `BulkDocsOptions::replication()` for replication-mode writes (which sets `new_edits: false`).

| Constructor | `new_edits` Value | Use Case |
|-------------|-------------------|----------|
| `BulkDocsOptions::new()` | `true` | Normal application writes. |
| `BulkDocsOptions::replication()` | `false` | Replication protocol writes. |

---

### AllDocsOptions

Options for querying all documents.

```rust
pub struct AllDocsOptions {
    pub start_key: Option<String>,
    pub end_key: Option<String>,
    pub key: Option<String>,
    pub keys: Option<Vec<String>>,
    pub include_docs: bool,
    pub descending: bool,
    pub skip: u64,
    pub limit: Option<u64>,
    pub inclusive_end: bool,
    pub conflicts: bool,
    pub update_seq: bool,
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `start_key` | `Option<String>` | `None` | Start of the key range (inclusive). |
| `end_key` | `Option<String>` | `None` | End of the key range (inclusive by default, see `inclusive_end`). |
| `key` | `Option<String>` | `None` | Return only the row matching this exact key. |
| `keys` | `Option<Vec<String>>` | `None` | Return only the rows matching these exact keys. |
| `include_docs` | `bool` | `false` | Include the full document body in each row. |
| `descending` | `bool` | `false` | Return rows in descending key order. |
| `skip` | `u64` | `0` | Number of rows to skip before returning results. |
| `limit` | `Option<u64>` | `None` | Maximum number of rows to return. |
| `inclusive_end` | `bool` | `true` (via `AllDocsOptions::new()`) | Whether the `end_key` is included in the range. |
| `conflicts` | `bool` | `false` | Include `_conflicts` for each document (requires `include_docs`). |
| `update_seq` | `bool` | `false` | Include the current `update_seq` in the response. |

**Note:** Use `AllDocsOptions::new()` instead of `Default::default()` to get `inclusive_end: true`, which matches CouchDB's default behavior.

---

### ChangesOptions

Options for the changes feed.

```rust
pub struct ChangesOptions {
    pub since: Seq,
    pub limit: Option<u64>,
    pub descending: bool,
    pub include_docs: bool,
    pub live: bool,
    pub doc_ids: Option<Vec<String>>,
    pub selector: Option<serde_json::Value>,
    pub conflicts: bool,
    pub style: ChangesStyle,
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `since` | `Seq` | `Seq::Num(0)` | Return changes after this sequence. Use `Seq::zero()` for all changes. |
| `limit` | `Option<u64>` | `None` | Maximum number of change events to return. |
| `descending` | `bool` | `false` | Return changes in reverse sequence order. |
| `include_docs` | `bool` | `false` | Include the full document body in each change event. |
| `live` | `bool` | `false` | Enable continuous (live) changes feed. |
| `doc_ids` | `Option<Vec<String>>` | `None` | Filter changes to only these document IDs. |
| `selector` | `Option<serde_json::Value>` | `None` | Mango selector to filter changes. Only changes matching the selector are returned. |
| `conflicts` | `bool` | `false` | Include conflicting revisions per change event. |
| `style` | `ChangesStyle` | `MainOnly` | `MainOnly` returns only the winning revision; `AllDocs` returns all leaf revisions. |

---

### FindOptions

Options for a Mango find query (from `rouchdb-query`).

```rust
pub struct FindOptions {
    pub selector: serde_json::Value,
    pub fields: Option<Vec<String>>,
    pub sort: Option<Vec<SortField>>,
    pub limit: Option<u64>,
    pub skip: Option<u64>,
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `selector` | `serde_json::Value` | `Value::Null` | The Mango selector (query) to match documents against. Must be a JSON object. |
| `fields` | `Option<Vec<String>>` | `None` | Field projection -- only include these fields in results. `_id` is always included. |
| `sort` | `Option<Vec<SortField>>` | `None` | Sort specification. Each entry is a field name or a `{field: direction}` map. |
| `limit` | `Option<u64>` | `None` | Maximum number of matching documents to return. |
| `skip` | `Option<u64>` | `None` | Number of matching documents to skip. |

#### SortField

```rust
pub enum SortField {
    Simple(String),
    WithDirection(HashMap<String, String>),
}
```

| Variant | Example JSON | Description |
|---------|-------------|-------------|
| `Simple(String)` | `"name"` | Sort by field in ascending order. |
| `WithDirection(HashMap<String, String>)` | `{"age": "desc"}` | Sort by field with explicit direction (`"asc"` or `"desc"`). |

---

### ViewQueryOptions

Options for map/reduce view queries (from `rouchdb-query`).

```rust
pub struct ViewQueryOptions {
    pub key: Option<serde_json::Value>,
    pub keys: Option<Vec<serde_json::Value>>,
    pub start_key: Option<serde_json::Value>,
    pub end_key: Option<serde_json::Value>,
    pub inclusive_end: bool,
    pub descending: bool,
    pub skip: u64,
    pub limit: Option<u64>,
    pub include_docs: bool,
    pub reduce: bool,
    pub group: bool,
    pub group_level: Option<u64>,
    pub stale: StaleOption,
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `key` | `Option<serde_json::Value>` | `None` | Return only rows with this exact key. |
| `keys` | `Option<Vec<serde_json::Value>>` | `None` | Return only rows matching any of these keys, in the given order. |
| `start_key` | `Option<serde_json::Value>` | `None` | Start of key range (inclusive). |
| `end_key` | `Option<serde_json::Value>` | `None` | End of key range (inclusive by default). |
| `inclusive_end` | `bool` | `true` (via `ViewQueryOptions::new()`) | Whether the `end_key` is included in the range. |
| `descending` | `bool` | `false` | Reverse row order. |
| `skip` | `u64` | `0` | Number of rows to skip. |
| `limit` | `Option<u64>` | `None` | Maximum number of rows to return. |
| `include_docs` | `bool` | `false` | Include full document body in each row. |
| `reduce` | `bool` | `false` | Whether to run the reduce function. |
| `group` | `bool` | `false` | Group results by key (requires `reduce: true`). |
| `group_level` | `Option<u64>` | `None` | Group to this many array elements of the key (requires `reduce: true`). |
| `stale` | `StaleOption` | `False` | `False` rebuilds the index before querying (default). `Ok` uses a potentially stale index. `UpdateAfter` returns stale results then rebuilds. |

---

### GetAttachmentOptions

Options for retrieving attachments.

```rust
pub struct GetAttachmentOptions {
    pub rev: Option<String>,
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `rev` | `Option<String>` | `None` | Retrieve the attachment from a specific revision. |

---

## Response Structs

### AllDocsResponse

```rust
pub struct AllDocsResponse {
    pub total_rows: u64,
    pub offset: u64,
    pub rows: Vec<AllDocsRow>,
    pub update_seq: Option<Seq>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `total_rows` | `u64` | Total number of non-deleted documents in the database. |
| `offset` | `u64` | Number of rows skipped. |
| `rows` | `Vec<AllDocsRow>` | The result rows. |
| `update_seq` | `Option<Seq>` | The current update sequence, present when `update_seq: true` was requested. |

### AllDocsRow

```rust
pub struct AllDocsRow {
    pub id: String,
    pub key: String,
    pub value: AllDocsRowValue,
    pub doc: Option<serde_json::Value>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | `String` | The document ID. |
| `key` | `String` | The row key (same as `id` for `all_docs`). |
| `value` | `AllDocsRowValue` | Contains the revision and optional deletion flag. |
| `doc` | `Option<serde_json::Value>` | Full document body, present only when `include_docs` is `true`. |

### AllDocsRowValue

```rust
pub struct AllDocsRowValue {
    pub rev: String,
    pub deleted: Option<bool>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `rev` | `String` | The winning revision string. |
| `deleted` | `Option<bool>` | Set to `Some(true)` for deleted documents. Omitted from JSON when `None`. |

---

### ChangesResponse

```rust
pub struct ChangesResponse {
    pub results: Vec<ChangeEvent>,
    pub last_seq: Seq,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `results` | `Vec<ChangeEvent>` | The list of change events. |
| `last_seq` | `Seq` | The sequence of the last change. Pass this as `since` for the next poll. |

### ChangeEvent

```rust
pub struct ChangeEvent {
    pub seq: Seq,
    pub id: String,
    pub changes: Vec<ChangeRev>,
    pub deleted: bool,
    pub doc: Option<serde_json::Value>,
    pub conflicts: Option<Vec<String>>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `seq` | `Seq` | The sequence number for this change. |
| `id` | `String` | The document ID. |
| `changes` | `Vec<ChangeRev>` | List of changed revision strings. |
| `deleted` | `bool` | `true` if the document was deleted. Defaults to `false`. |
| `doc` | `Option<serde_json::Value>` | Full document body, present only when `include_docs` is `true`. |
| `conflicts` | `Option<Vec<String>>` | Conflicting revisions, present when `conflicts: true` was requested. |

### ChangeRev

```rust
pub struct ChangeRev {
    pub rev: String,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `rev` | `String` | The revision string for this change. |

---

### FindResponse

Result of a Mango find query (from `rouchdb-query`).

```rust
pub struct FindResponse {
    pub docs: Vec<serde_json::Value>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `docs` | `Vec<serde_json::Value>` | Matching documents as JSON values. Includes `_id` and `_rev` fields. If `fields` was specified in `FindOptions`, only the projected fields are present (plus `_id`). |

---

### ViewResult

Result of a map/reduce view query (from `rouchdb-query`).

```rust
pub struct ViewResult {
    pub total_rows: u64,
    pub offset: u64,
    pub rows: Vec<ViewRow>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `total_rows` | `u64` | Total number of rows emitted by the map function (before skip/limit). |
| `offset` | `u64` | Number of rows skipped. |
| `rows` | `Vec<ViewRow>` | The result rows. |

### ViewRow

```rust
pub struct ViewRow {
    pub id: Option<String>,
    pub key: serde_json::Value,
    pub value: serde_json::Value,
    pub doc: Option<serde_json::Value>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | `Option<String>` | The source document ID. `None` for reduce results. |
| `key` | `serde_json::Value` | The emitted key. |
| `value` | `serde_json::Value` | The emitted value (or reduced value). |
| `doc` | `Option<serde_json::Value>` | Full document body, present only when `include_docs` is `true`. |

---

## Replication Types

### BulkGetItem

A request to fetch a specific document (optionally at a specific revision).

```rust
pub struct BulkGetItem {
    pub id: String,
    pub rev: Option<String>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | `String` | The document ID to fetch. |
| `rev` | `Option<String>` | Specific revision to fetch. If `None`, returns the winning revision. |

### BulkGetResponse

```rust
pub struct BulkGetResponse {
    pub results: Vec<BulkGetResult>,
}
```

### BulkGetResult

```rust
pub struct BulkGetResult {
    pub id: String,
    pub docs: Vec<BulkGetDoc>,
}
```

### BulkGetDoc

```rust
pub struct BulkGetDoc {
    pub ok: Option<serde_json::Value>,
    pub error: Option<BulkGetError>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `ok` | `Option<serde_json::Value>` | The document JSON if fetch succeeded. |
| `error` | `Option<BulkGetError>` | Error details if fetch failed. |

### BulkGetError

```rust
pub struct BulkGetError {
    pub id: String,
    pub rev: String,
    pub error: String,
    pub reason: String,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | `String` | The document ID that failed. |
| `rev` | `String` | The revision that was requested. |
| `error` | `String` | Error type (e.g., `"not_found"`). |
| `reason` | `String` | Human-readable error description. |

### RevsDiffResponse

```rust
pub struct RevsDiffResponse {
    pub results: HashMap<String, RevsDiffResult>,
}
```

The `results` field is flattened during serialization (`#[serde(flatten)]`), so it serializes as a flat JSON object keyed by document ID.

### RevsDiffResult

```rust
pub struct RevsDiffResult {
    pub missing: Vec<String>,
    pub possible_ancestors: Vec<String>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `missing` | `Vec<String>` | Revisions the adapter does not have. |
| `possible_ancestors` | `Vec<String>` | Revisions the adapter does have that could be ancestors of the missing ones. Empty vec is omitted from JSON. |

---

## ReduceFn

Built-in reduce functions for map/reduce views (from `rouchdb-query`).

```rust
pub enum ReduceFn {
    Sum,
    Count,
    Stats,
    Custom(Box<dyn Fn(&[serde_json::Value], &[serde_json::Value], bool) -> serde_json::Value>),
}
```

| Variant | Description |
|---------|-------------|
| `Sum` | Sum all numeric values. |
| `Count` | Count the number of rows. |
| `Stats` | Compute statistics: `sum`, `count`, `min`, `max`, `sumsqr`. |
| `Custom(Fn)` | Custom reduce function. Arguments: `(keys, values, rereduce)`. |
