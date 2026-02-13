use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::error::AppError;
use crate::state::AppState;

/// GET /{db} — database info with CouchDB-compatible fields.
pub async fn get_db_info(
    State(state): State<AppState>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }

    let info = state.db.info().await?;
    Ok(Json(serde_json::json!({
        "db_name": info.db_name,
        "doc_count": info.doc_count,
        "doc_del_count": 0,
        "update_seq": info.update_seq,
        "purge_seq": 0,
        "compact_running": false,
        "disk_size": 0,
        "data_size": 0,
        "instance_start_time": "0",
        "disk_format_version": 8,
        "committed_update_seq": info.update_seq,
        "compacted_seq": 0,
        "uuid": "rouchdb",
        "sizes": {
            "file": 0,
            "external": 0,
            "active": 0,
        },
        "props": {},
    })))
}

/// PUT /{db} — stub: returns 201 if name matches (already exists).
pub async fn put_db(
    State(state): State<AppState>,
    Path(db): Path<String>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::BadRequest(
            format!("Cannot create database {db}: single-db mode"),
        )));
    }

    Ok((StatusCode::CREATED, Json(serde_json::json!({"ok": true}))))
}

/// DELETE /{db} — delete a database.
pub async fn delete_db(
    State(state): State<AppState>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }

    state.db.destroy().await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /{db} — create a new document with auto-generated ID.
pub async fn post_doc(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }

    let result = state.db.post(body).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": result.ok,
            "id": result.id,
            "rev": result.rev,
        })),
    ))
}
