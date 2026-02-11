# Attachments

RouchDB supports binary attachments on documents, following the CouchDB attachment model. Attachments are binary blobs (images, PDFs, any file) stored alongside a document and identified by a name. Each attachment has a content type, a length, and a digest for deduplication.

## Overview

Attachments are accessed through the `Database` methods `put_attachment`, `get_attachment`, `get_attachment_with_opts`, and `remove_attachment`.

## Storing an Attachment

Use `put_attachment` to add or replace an attachment on a document. You must provide the current document revision, just like an update.

```rust
use rouchdb::Database;
use serde_json::json;

let db = Database::memory("mydb");

// First, create the document
let result = db.put("photo:1", json!({"title": "Sunset"})).await?;
let rev = result.rev.unwrap();

// Attach a binary file
let image_bytes: Vec<u8> = std::fs::read("sunset.jpg")?;

let att_result = db.put_attachment(
    "photo:1",       // document ID
    "sunset.jpg",    // attachment name
    &rev,            // current document revision
    image_bytes,     // raw bytes
    "image/jpeg",    // MIME content type
).await?;

assert!(att_result.ok);
println!("New rev after attachment: {}", att_result.rev.unwrap());
```

The `put_attachment` method signature:

```rust
async fn put_attachment(
    &self,
    doc_id: &str,
    att_id: &str,
    rev: &str,
    data: Vec<u8>,
    content_type: &str,
) -> Result<DocResult>;
```

| Parameter | Description |
|-----------|-------------|
| `doc_id` | The document ID to attach to. |
| `att_id` | The attachment name (e.g., `"photo.png"`, `"report.pdf"`). |
| `rev` | The current revision of the document. A new revision is created. |
| `data` | The raw binary data as `Vec<u8>`. |
| `content_type` | MIME type string (e.g., `"image/png"`, `"application/pdf"`). |

The returned `DocResult` contains the new revision string. Subsequent updates to the document or its attachments must use this new revision.

## Retrieving an Attachment

Use `get_attachment` to read the raw bytes of an attachment:

```rust
use rouchdb::GetAttachmentOptions;

let bytes = db.get_attachment_with_opts(
    "photo:1",
    "sunset.jpg",
    GetAttachmentOptions::default(),
).await?;

println!("Attachment size: {} bytes", bytes.len());

// Write to a file
std::fs::write("downloaded_sunset.jpg", &bytes)?;
```

The `get_attachment` method signature:

```rust
async fn get_attachment(
    &self,
    doc_id: &str,
    att_id: &str,
    opts: GetAttachmentOptions,
) -> Result<Vec<u8>>;
```

### GetAttachmentOptions

```rust
use rouchdb::GetAttachmentOptions;

let opts = GetAttachmentOptions {
    rev: Some("2-abc123...".into()), // fetch from a specific revision
};
```

| Field | Type | Description |
|-------|------|-------------|
| `rev` | `Option<String>` | Retrieve the attachment from a specific document revision. If `None`, uses the current winning revision. |

## AttachmentMeta

When you retrieve a document, its `attachments` field is a `HashMap<String, AttachmentMeta>`. The metadata describes each attachment without including the raw data (unless explicitly requested):

```rust
use rouchdb::AttachmentMeta;

let doc = db.get("photo:1").await?;

for (name, meta) in &doc.attachments {
    println!("Attachment: {}", name);
    println!("  Content-Type: {}", meta.content_type);
    println!("  Digest:       {}", meta.digest);
    println!("  Length:        {} bytes", meta.length);
    println!("  Stub:         {}", meta.stub);
}
```

The `AttachmentMeta` fields:

| Field | Type | Description |
|-------|------|-------------|
| `content_type` | `String` | The MIME type (e.g., `"image/png"`). |
| `digest` | `String` | A content-addressed hash of the data (e.g., `"md5-abc123..."`). |
| `length` | `u64` | Size of the attachment in bytes. |
| `stub` | `bool` | If `true`, the `data` field is absent and only metadata is present. This is the common case when reading documents. |
| `data` | `Option<Vec<u8>>` | The raw binary data, present only when explicitly included. |

## Digest-Based Deduplication

Attachments are stored by their content digest. If two documents have the same attachment data, the bytes are stored only once. This is particularly beneficial during replication: if the target already has the attachment data (identified by digest), it does not need to be transferred again.

The digest is computed when the attachment is written and stored in the `AttachmentMeta`. It follows CouchDB's format (e.g., `"md5-<base64hash>"`).

## Multiple Attachments

A document can have any number of named attachments. Each `put_attachment` call creates a new document revision:

```rust
let r1 = db.put("report:q1", json!({"title": "Q1 Report"})).await?;
let rev1 = r1.rev.unwrap();

// Add first attachment
let r2 = db.adapter().put_attachment(
    "report:q1", "summary.pdf", &rev1,
    pdf_bytes, "application/pdf",
).await?;
let rev2 = r2.rev.unwrap();

// Add second attachment (using the new revision)
let r3 = db.adapter().put_attachment(
    "report:q1", "charts.png", &rev2,
    png_bytes, "image/png",
).await?;

// The document now has two attachments
let doc = db.get("report:q1").await?;
assert_eq!(doc.attachments.len(), 2);
```

## Attachments in Document JSON

When converting a document to JSON with `doc.to_json()`, attachments appear under the `_attachments` key:

```json
{
    "_id": "photo:1",
    "_rev": "2-def456...",
    "title": "Sunset",
    "_attachments": {
        "sunset.jpg": {
            "content_type": "image/jpeg",
            "digest": "md5-abc123...",
            "length": 524288,
            "stub": true
        }
    }
}
```

When creating documents from JSON using `Document::from_json()`, the `_attachments` field is automatically parsed into the `attachments` `HashMap`.

## Inline Base64 Attachments

When creating documents from JSON (e.g., from CouchDB responses or imported data), attachments can be included inline as Base64-encoded strings under the `_attachments` key:

```json
{
    "_id": "photo:1",
    "title": "Sunset",
    "_attachments": {
        "image.txt": {
            "content_type": "text/plain",
            "data": "SGVsbG8gV29ybGQ="
        }
    }
}
```

When `Document::from_json()` encounters a `data` field as a Base64 string, it automatically decodes it into the `AttachmentMeta.data` field as `Vec<u8>`. This is compatible with CouchDB's inline attachment format.

```rust
let json = serde_json::json!({
    "_id": "doc1",
    "_attachments": {
        "note.txt": {
            "content_type": "text/plain",
            "data": "SGVsbG8gV29ybGQ="
        }
    }
});

let doc = Document::from_json(json)?;
let att = &doc.attachments["note.txt"];
assert_eq!(att.data.as_ref().unwrap(), b"Hello World");
assert_eq!(att.content_type, "text/plain");
```

## Removing an Attachment

Use `remove_attachment()` to remove an attachment from a document, creating a new revision:

```rust
let result = db.remove_attachment("photo:1", "sunset.jpg", &rev).await?;
// The document now has one fewer attachment
```

## Attachments and Replication

Attachments participate in replication automatically. When a document with attachments is replicated, the attachment data is included in the transfer. Thanks to digest-based deduplication, attachments that already exist on the target are not transferred again, which saves bandwidth for large binary files.
