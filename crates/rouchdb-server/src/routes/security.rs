use axum::Json;
use axum::extract::{Path, State};

use rouchdb::SecurityDocument;

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
    Json(body): Json<SecurityDocument>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    state.db.put_security(body).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}
