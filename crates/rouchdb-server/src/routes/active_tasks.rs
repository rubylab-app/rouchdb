use axum::Json;

/// GET /_active_tasks — list running tasks (stub: no tasks).
pub async fn get_active_tasks() -> Json<serde_json::Value> {
    Json(serde_json::json!([]))
}
