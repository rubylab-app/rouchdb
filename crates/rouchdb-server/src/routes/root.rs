use axum::Json;
use axum::extract::State;

use crate::error::AppError;
use crate::state::AppState;

/// GET / — CouchDB welcome message.
///
/// Reports CouchDB 3.3.3 so Fauxton enables all UI panels, and the uuid of
/// the served database (see [`AppState::uuid`]).
pub async fn root_info(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(serde_json::json!({
        "couchdb": "Welcome",
        "version": "3.3.3",
        "vendor": {
            "name": "RouchDB",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "features": ["access-ready", "partitioned", "pluggable-storage-engines", "reshard", "scheduler"],
        "git_sha": "00000000",
        "uuid": state.uuid().await?,
    })))
}
