use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rouchdb_core::error::RouchError;
use serde_json::json;

use super::set_location;
use crate::error::AppError;
use crate::state::{AppState, db_not_found};

/// GET /{db} — database info with CouchDB-compatible fields.
pub async fn get_db_info(
    State(state): State<AppState>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;

    let info = state.db.info().await?;
    Ok(Json(serde_json::json!({
        "db_name": info.db_name,
        "doc_count": info.doc_count,
        "doc_del_count": info.doc_del_count,
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

/// PUT /{db} — create the database.
///
/// In single-db mode the database exists from startup, so this only succeeds
/// after a `DELETE /{db}` (201, the database is back and empty); otherwise it
/// is CouchDB's 412 `file_exists`. Other names cannot be created.
pub async fn put_db(
    State(state): State<AppState>,
    Path(db): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if db != state.db_name {
        return Err(AppError(RouchError::BadRequest(format!(
            "Cannot create database {db}: single-db mode"
        ))));
    }
    if !state.set_deleted(false) {
        return Err(AppError(RouchError::DatabaseExists(
            "The database could not be created, the file already exists.".into(),
        )));
    }
    let mut resp = (StatusCode::CREATED, Json(json!({"ok": true}))).into_response();
    set_location(&mut resp, &headers, &[&db]);
    Ok(resp)
}

/// DELETE /{db} — delete the database: its data is destroyed and every
/// database route answers 404 until a `PUT /{db}` creates it again.
///
/// The server keeps serving the same storage, so a restart brings the (empty)
/// database back.
pub async fn delete_db(
    State(state): State<AppState>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;
    // Mark it deleted first so no new request writes into it while it is
    // being destroyed; if destroying fails, nothing was removed.
    if state.set_deleted(true) {
        return Err(db_not_found());
    }
    if let Err(e) = state.db.destroy().await {
        state.set_deleted(false);
        return Err(AppError(e));
    }
    Ok(Json(json!({"ok": true})))
}

/// POST /{db} — create a new document with auto-generated ID.
pub async fn post_doc(
    State(state): State<AppState>,
    Path(db): Path<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    state.check_db(&db)?;

    let result = state.db.post(body).await?;
    let mut resp = (
        StatusCode::CREATED,
        Json(json!({
            "ok": result.ok,
            "id": result.id,
            "rev": result.rev,
        })),
    )
        .into_response();
    set_location(&mut resp, &headers, &[&db, &result.id]);
    Ok(resp)
}
