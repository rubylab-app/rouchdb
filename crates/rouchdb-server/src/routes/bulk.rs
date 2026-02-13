use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use rouchdb::{BulkDocsOptions, Document};

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

/// POST /{db}/_bulk_docs — write multiple documents.
pub async fn bulk_docs(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(body): Json<BulkDocsBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }

    let docs: Vec<Document> = body
        .docs
        .into_iter()
        .map(Document::from_json)
        .collect::<rouchdb::Result<Vec<_>>>()?;

    let opts = BulkDocsOptions {
        new_edits: body.new_edits,
    };

    let results = state.db.bulk_docs(docs, opts).await?;

    let response: Vec<serde_json::Value> = results
        .into_iter()
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
