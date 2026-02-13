use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::error::AppError;
use crate::state::AppState;

/// POST /{db}/_compact — compact the database.
pub async fn compact(
    State(state): State<AppState>,
    Path(db): Path<String>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }

    state.db.compact().await?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"ok": true}))))
}
