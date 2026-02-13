use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct ViewQuery {
    pub key: Option<String>,
    pub keys: Option<String>,
    pub startkey: Option<String>,
    pub start_key: Option<String>,
    pub endkey: Option<String>,
    pub end_key: Option<String>,
    pub limit: Option<u64>,
    pub skip: Option<u64>,
    pub descending: Option<bool>,
    pub include_docs: Option<bool>,
    pub inclusive_end: Option<bool>,
    pub reduce: Option<bool>,
    pub group: Option<bool>,
    pub group_level: Option<u64>,
    pub stale: Option<String>,
    pub update: Option<String>,
}

fn validate_db(db: &str, state: &AppState) -> Result<(), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }
    Ok(())
}

/// GET /{db}/_design/{ddoc}/_view/{view} — query a view.
///
/// RouchDB views use Rust closures, not JavaScript. Design document views
/// stored as JS strings cannot be executed server-side.
pub async fn get_view(
    State(state): State<AppState>,
    Path((db, ddoc, view)): Path<(String, String, String)>,
    Query(_query): Query<ViewQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let design = state.db.get_design(&ddoc).await?;
    if !design.views.contains_key(&view) {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("missing named view: {view}"),
        )));
    }

    Err(AppError(rouchdb_core::error::RouchError::BadRequest(
        "JavaScript views are not supported. RouchDB uses Rust closures for map/reduce."
            .to_string(),
    )))
}

/// POST /{db}/_design/{ddoc}/_view/{view} — query a view with POST body.
pub async fn post_view(
    State(state): State<AppState>,
    Path((db, ddoc, view)): Path<(String, String, String)>,
    Json(_body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let design = state.db.get_design(&ddoc).await?;
    if !design.views.contains_key(&view) {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("missing named view: {view}"),
        )));
    }

    Err(AppError(rouchdb_core::error::RouchError::BadRequest(
        "JavaScript views are not supported. RouchDB uses Rust closures for map/reduce."
            .to_string(),
    )))
}

/// GET /{db}/_design/{ddoc}/_info — get design document info.
pub async fn get_design_info(
    State(state): State<AppState>,
    Path((db, ddoc)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    // Verify the design doc exists
    let design = state.db.get_design(&ddoc).await?;
    let view_count = design.views.len();

    Ok(Json(serde_json::json!({
        "name": design.name(),
        "view_index": {
            "compact_running": false,
            "language": design.language.as_deref().unwrap_or("javascript"),
            "purge_seq": 0,
            "signature": "0",
            "sizes": {
                "active": 0,
                "disk": 0,
                "external": 0,
            },
            "updater_running": false,
            "waiting_clients": 0,
            "waiting_commit": false,
            "updates_pending": {
                "minimum": 0,
                "preferred": 0,
                "total": 0,
            },
            "collator_versions": ["153.104"],
            "view_count": view_count,
        },
    })))
}
