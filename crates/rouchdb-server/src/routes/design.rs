use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;

use super::document::resolve_rev;
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct DesignDeleteQuery {
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

/// GET /{db}/_design/{ddoc} — get a design document.
pub async fn get_design(
    State(state): State<AppState>,
    Path((db, ddoc)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let design = state.db.get_design(&ddoc).await?;
    Ok(Json(design.to_json()))
}

/// PUT /{db}/_design/{ddoc} — create or update a design document.
pub async fn put_design(
    State(state): State<AppState>,
    Path((db, ddoc)): Path<(String, String)>,
    Query(query): Query<DesignDeleteQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    validate_db(&db, &state)?;
    let mut obj = super::json_object_body(&body)?;

    // Parse the body as a design document, injecting _id and the revision
    // from `?rev`, `_rev` or `If-Match`.
    let body_rev = obj.get("_rev").and_then(|v| v.as_str()).map(String::from);
    if let Some(rev) = resolve_rev(query.rev, body_rev, &headers)? {
        obj.insert("_rev".to_string(), serde_json::Value::String(rev));
    }
    obj.insert(
        "_id".to_string(),
        serde_json::Value::String(format!("_design/{ddoc}")),
    );
    let doc_json = serde_json::Value::Object(obj);

    let design = rouchdb::DesignDocument::from_json(doc_json).map_err(|_| {
        AppError(rouchdb_core::error::RouchError::BadRequest(
            "Invalid design document".to_string(),
        ))
    })?;

    let result = state.db.put_design(design).await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": result.ok,
            "id": result.id,
            "rev": result.rev,
        })),
    ))
}

/// DELETE /{db}/_design/{ddoc} — delete a design document.
pub async fn delete_design(
    State(state): State<AppState>,
    Path((db, ddoc)): Path<(String, String)>,
    Query(query): Query<DesignDeleteQuery>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    // Without any revision CouchDB reports a conflict.
    let rev = resolve_rev(query.rev, None, &headers)?
        .ok_or(AppError(rouchdb_core::error::RouchError::Conflict))?;

    let result = state.db.delete_design(&ddoc, &rev).await?;
    Ok(Json(serde_json::json!({
        "ok": result.ok,
        "id": result.id,
        "rev": result.rev,
    })))
}
