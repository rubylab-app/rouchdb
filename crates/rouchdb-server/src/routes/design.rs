use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

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
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    validate_db(&db, &state)?;

    // Parse the body as a design document, injecting _id
    let mut doc_json = body;
    if let Some(obj) = doc_json.as_object_mut() {
        obj.insert(
            "_id".to_string(),
            serde_json::Value::String(format!("_design/{ddoc}")),
        );
    }

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
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let rev = query.rev.ok_or_else(|| {
        AppError(rouchdb_core::error::RouchError::BadRequest(
            "Missing rev parameter".to_string(),
        ))
    })?;

    let result = state.db.delete_design(&ddoc, &rev).await?;
    Ok(Json(serde_json::json!({
        "ok": result.ok,
        "id": result.id,
        "rev": result.rev,
    })))
}
