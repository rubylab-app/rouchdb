use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct AttachmentQuery {
    pub rev: Option<String>,
}

fn validate_db(db: &str, state: &AppState) -> Result<(), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }
    Ok(())
}

/// GET /{db}/{docid}/{attname} — download an attachment.
pub async fn get_attachment(
    State(state): State<AppState>,
    Path((db, docid, attname)): Path<(String, String, String)>,
) -> Result<Response, AppError> {
    validate_db(&db, &state)?;

    let data = state.db.get_attachment(&docid, &attname).await?;

    // Try to guess content type from the attachment name
    let content_type = mime_guess::from_path(&attname)
        .first_or_octet_stream()
        .to_string();

    Ok((StatusCode::OK, [("content-type", content_type)], data).into_response())
}

/// PUT /{db}/{docid}/{attname}?rev=... — upload an attachment.
pub async fn put_attachment(
    State(state): State<AppState>,
    Path((db, docid, attname)): Path<(String, String, String)>,
    Query(query): Query<AttachmentQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, axum::Json<serde_json::Value>), AppError> {
    validate_db(&db, &state)?;

    let rev = query.rev.ok_or_else(|| {
        AppError(rouchdb_core::error::RouchError::BadRequest(
            "Missing rev parameter".to_string(),
        ))
    })?;

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    let result = state
        .db
        .put_attachment(&docid, &attname, &rev, body.to_vec(), content_type)
        .await?;

    Ok((
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "ok": result.ok,
            "id": result.id,
            "rev": result.rev,
        })),
    ))
}

/// DELETE /{db}/{docid}/{attname}?rev=... — delete an attachment.
pub async fn delete_attachment(
    State(state): State<AppState>,
    Path((db, docid, attname)): Path<(String, String, String)>,
    Query(query): Query<AttachmentQuery>,
) -> Result<axum::Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let rev = query.rev.ok_or_else(|| {
        AppError(rouchdb_core::error::RouchError::BadRequest(
            "Missing rev parameter".to_string(),
        ))
    })?;

    let result = state.db.remove_attachment(&docid, &attname, &rev).await?;

    Ok(axum::Json(serde_json::json!({
        "ok": result.ok,
        "id": result.id,
        "rev": result.rev,
    })))
}
