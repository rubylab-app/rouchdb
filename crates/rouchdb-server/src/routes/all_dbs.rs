use axum::Json;
use axum::extract::State;

use crate::state::AppState;

/// GET /_all_dbs — the served database, unless it was deleted.
pub async fn all_dbs(State(state): State<AppState>) -> Json<serde_json::Value> {
    if state.db_exists() {
        Json(serde_json::json!([state.db_name]))
    } else {
        Json(serde_json::json!([]))
    }
}
