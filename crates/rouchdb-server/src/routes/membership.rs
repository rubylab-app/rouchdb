use axum::Json;

/// GET /_membership — list cluster nodes (stub: single node).
pub async fn get_membership() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "all_nodes": ["nonode@nohost"],
        "cluster_nodes": ["nonode@nohost"],
    }))
}
