use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::Deserialize;

use rouchdb::{FindOptions, IndexDefinition};
use rouchdb_core::error::RouchError;

use crate::error::AppError;
use crate::state::AppState;

fn validate_db(db: &str, state: &AppState) -> Result<(), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }
    Ok(())
}

/// CouchDB's `_find` limit when the request does not set one.
const DEFAULT_FIND_LIMIT: u64 = 25;

/// Bookmarks are opaque to clients; ours encode how many results of the
/// (deterministically ordered) query were already returned.
fn encode_bookmark(offset: u64) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{{\"skip\":{offset}}}"))
}

fn decode_bookmark(bookmark: &serde_json::Value) -> Option<u64> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(bookmark.as_str()?)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("skip")?.as_u64()
}

/// POST /{db}/_find — run a Mango query.
pub async fn find(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(mut body): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    validate_db(&db, &state)?;

    let offset = match body.as_object_mut().and_then(|o| o.remove("bookmark")) {
        None | Some(serde_json::Value::Null) => None,
        Some(bookmark) => match decode_bookmark(&bookmark) {
            Some(offset) => Some(offset),
            None => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_bookmark",
                        "reason": format!("Invalid bookmark value: {bookmark}"),
                    })),
                )
                    .into_response());
            }
        },
    };
    let mut opts: FindOptions = serde_json::from_value(body)
        .map_err(|e| AppError(RouchError::BadRequest(format!("invalid query: {e}"))))?;

    // A bookmark resumes after the results already returned; `skip` applies
    // on top of it, as in CouchDB.
    let skip = offset.unwrap_or(0) + opts.skip.unwrap_or(0);
    opts.skip = Some(skip);
    opts.limit = Some(opts.limit.unwrap_or(DEFAULT_FIND_LIMIT));

    let response = state.db.find(opts).await?;
    let bookmark = if response.docs.is_empty() && offset.is_none() {
        "nil".to_string()
    } else {
        encode_bookmark(skip + response.docs.len() as u64)
    };
    Ok(Json(serde_json::json!({
        "docs": response.docs,
        "bookmark": bookmark,
    }))
    .into_response())
}

#[derive(Deserialize)]
pub struct CreateIndexBody {
    pub index: IndexFieldsBody,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub ddoc: Option<String>,
}

#[derive(Deserialize)]
pub struct IndexFieldsBody {
    pub fields: Vec<rouchdb::SortField>,
}

/// POST /{db}/_index — create a Mango index.
pub async fn create_index(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(body): Json<CreateIndexBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    validate_db(&db, &state)?;

    let def = IndexDefinition {
        name: body.name.unwrap_or_default(),
        fields: body.index.fields,
        ddoc: body.ddoc,
    };

    let result = state.db.create_index(def).await?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "result": result.result,
            "id": result.name,
            "name": result.name,
        })),
    ))
}

/// GET /{db}/_index — list all indexes.
pub async fn get_indexes(
    State(state): State<AppState>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let indexes = state.db.get_indexes().await;

    // Always include the special _all_docs index
    let mut all_indexes = vec![serde_json::json!({
        "ddoc": null,
        "name": "_all_docs",
        "type": "special",
        "def": { "fields": [{"_id": "asc"}] },
    })];

    for idx in indexes {
        all_indexes.push(serde_json::json!({
            "ddoc": idx.ddoc,
            "name": idx.name,
            "type": "json",
            "def": { "fields": idx.def.fields },
        }));
    }

    Ok(Json(serde_json::json!({
        "total_rows": all_indexes.len(),
        "indexes": all_indexes,
    })))
}

/// DELETE /{db}/_index/{ddoc}/json/{name} — delete an index.
pub async fn delete_index(
    State(state): State<AppState>,
    Path((db, _ddoc, _itype, name)): Path<(String, String, String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;
    state.db.delete_index(&name).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct BulkDeleteIndexBody {
    pub docids: Vec<String>,
}

/// POST /{db}/_index/_bulk_delete — bulk delete indexes.
pub async fn bulk_delete_indexes(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(body): Json<BulkDeleteIndexBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let mut success = Vec::new();
    let mut fail = Vec::new();

    for name in body.docids {
        match state.db.delete_index(&name).await {
            Ok(()) => success.push(serde_json::json!({"id": name, "ok": true})),
            Err(e) => fail.push(serde_json::json!({"id": name, "error": e.to_string()})),
        }
    }

    Ok(Json(serde_json::json!({
        "success": success,
        "fail": fail,
    })))
}

/// POST /{db}/_explain — explain query execution plan.
pub async fn explain(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(opts): Json<FindOptions>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;
    let response = state.db.explain(opts).await;
    Ok(Json(serde_json::to_value(&response).unwrap()))
}
