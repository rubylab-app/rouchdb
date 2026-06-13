use axum::Json;
use axum::extract::{Path, State};

use rouchdb::SecurityDocument;

use crate::error::AppError;
use crate::state::AppState;
use rouchdb_core::error::RouchError;

fn validate_db(db: &str, state: &AppState) -> Result<(), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }
    Ok(())
}

/// GET /{db}/_security — get database security document.
pub async fn get_security(
    State(state): State<AppState>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let sec = state.db.get_security().await?;
    Ok(Json(serde_json::to_value(&sec).unwrap()))
}

/// PUT /{db}/_security — update database security document.
pub async fn put_security(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(raw): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    // Parse here (instead of via the extractor) so a malformed body yields the
    // standard CouchDB error JSON rather than axum's default rejection. Unknown
    // top-level fields are preserved via SecurityDocument::extra.
    let body: SecurityDocument = serde_json::from_value(raw)
        .map_err(|e| AppError(RouchError::BadRequest(format!("invalid security doc: {e}"))))?;

    state.db.put_security(body).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}
