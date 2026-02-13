use axum::Json;
use axum::extract::State;

use crate::state::AppState;

/// GET /_all_dbs — returns the single database name.
pub async fn all_dbs(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!([state.db_name]))
}
