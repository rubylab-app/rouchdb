use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use super::json_required;
use crate::error::AppError;
use crate::state::AppState;

/// POST /{db}/_compact — compact the database.
///
/// Like CouchDB, the request must be sent as `application/json` (it has no
/// body), otherwise it is a 415.
pub async fn compact(
    State(state): State<AppState>,
    Path(db): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    state.check_db(&db)?;
    if let Some(rejection) = json_required(&headers) {
        return Ok(rejection);
    }

    state.db.compact().await?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"ok": true}))).into_response())
}
