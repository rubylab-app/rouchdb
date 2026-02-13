use axum::Json;

/// GET / — CouchDB welcome message.
///
/// Reports CouchDB 3.3.3 so Fauxton enables all UI panels.
pub async fn root_info() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "couchdb": "Welcome",
        "version": "3.3.3",
        "vendor": {
            "name": "RouchDB",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "features": ["access-ready", "partitioned", "pluggable-storage-engines", "reshard", "scheduler"],
        "git_sha": "00000000",
        "uuid": "rouchdb",
    }))
}
