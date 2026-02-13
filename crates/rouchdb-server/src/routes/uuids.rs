use axum::Json;
use axum::extract::Query;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct UuidsQuery {
    #[serde(default = "default_count")]
    pub count: usize,
}

fn default_count() -> usize {
    1
}

/// GET /_uuids?count=N — generate UUIDs.
pub async fn get_uuids(Query(query): Query<UuidsQuery>) -> Json<serde_json::Value> {
    let count = query.count.min(1000);
    let uuids: Vec<String> = (0..count)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    Json(serde_json::json!({ "uuids": uuids }))
}
