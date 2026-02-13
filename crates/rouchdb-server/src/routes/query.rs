use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use rouchdb::{FindOptions, IndexDefinition};

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

/// POST /{db}/_find — run a Mango query.
pub async fn find(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(opts): Json<FindOptions>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;
    let response = state.db.find(opts).await?;
    Ok(Json(serde_json::json!({
        "docs": response.docs,
        "bookmark": "nil",
    })))
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
