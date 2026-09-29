use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use rouchdb::{BulkDocsOptions, Document, Revision};
use rouchdb_core::error::RouchError;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct BulkDocsBody {
    pub docs: Vec<serde_json::Value>,
    #[serde(default = "default_new_edits")]
    pub new_edits: bool,
}

fn default_new_edits() -> bool {
    true
}

/// Validate a document of a `_bulk_docs` request the way CouchDB does before
/// writing anything: an invalid id or special member rejects the whole
/// request, as does a replicated (`new_edits: false`) document without a
/// revision.
fn validate(json: &serde_json::Value, doc: &Document, new_edits: bool) -> Result<(), AppError> {
    let mut doc = doc.clone();
    doc.prepare_for_write()?;
    if !new_edits && json.get("_rev").is_none() && json.get("_revisions").is_none() {
        return Err(AppError(RouchError::BadRequest(
            "When `new_edits: false`, the document needs `_rev` or `_revisions` specified".into(),
        )));
    }
    Ok(())
}

/// The leaf revision named by a `_revisions` history (`start` and the first
/// of `ids`), which CouchDB uses when a replicated document has no `_rev`.
fn revision_from_history(json: &serde_json::Value) -> Option<Revision> {
    let history = json.get("_revisions")?;
    let start = history.get("start")?.as_u64()?;
    let leaf = history.get("ids")?.as_array()?.first()?.as_str()?;
    Some(Revision::new(start, leaf.to_string()))
}

/// POST /{db}/_bulk_docs — write multiple documents.
///
/// With `new_edits: false` (replication) the response only lists the
/// documents that could not be written, so it is `[]` when all were, as in
/// CouchDB.
pub async fn bulk_docs(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(body): Json<BulkDocsBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    state.check_db(&db)?;

    let mut docs = Vec::with_capacity(body.docs.len());
    for json in body.docs {
        let mut doc = Document::from_json(json.clone())?;
        validate(&json, &doc, body.new_edits)?;
        if !body.new_edits && doc.rev.is_none() {
            doc.rev = revision_from_history(&json);
        }
        docs.push(doc);
    }

    let opts = BulkDocsOptions {
        new_edits: body.new_edits,
    };

    let results = state.db.bulk_docs(docs, opts).await?;

    let response: Vec<serde_json::Value> = results
        .into_iter()
        .filter(|r| body.new_edits || !r.ok)
        .map(|r| {
            if r.ok {
                serde_json::json!({
                    "ok": true,
                    "id": r.id,
                    "rev": r.rev,
                })
            } else {
                serde_json::json!({
                    "id": r.id,
                    "error": r.error,
                    "reason": r.reason,
                })
            }
        })
        .collect();

    Ok((StatusCode::CREATED, Json(serde_json::json!(response))))
}
