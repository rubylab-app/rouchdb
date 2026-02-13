use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use rouchdb::AllDocsOptions;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct AllDocsQuery {
    pub include_docs: Option<bool>,
    pub startkey: Option<String>,
    pub start_key: Option<String>,
    pub endkey: Option<String>,
    pub end_key: Option<String>,
    pub key: Option<String>,
    pub limit: Option<u64>,
    pub skip: Option<u64>,
    pub descending: Option<bool>,
    pub inclusive_end: Option<bool>,
    pub conflicts: Option<bool>,
    pub update_seq: Option<bool>,
}

impl AllDocsQuery {
    fn into_options(self, keys: Option<Vec<String>>) -> AllDocsOptions {
        AllDocsOptions {
            include_docs: self.include_docs.unwrap_or(false),
            start_key: self.startkey.or(self.start_key),
            end_key: self.endkey.or(self.end_key),
            key: self.key,
            keys,
            limit: self.limit,
            skip: self.skip.unwrap_or(0),
            descending: self.descending.unwrap_or(false),
            inclusive_end: self.inclusive_end.unwrap_or(true),
            conflicts: self.conflicts.unwrap_or(false),
            update_seq: self.update_seq.unwrap_or(false),
        }
    }
}

fn validate_db(db: &str, state: &AppState) -> Result<(), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }
    Ok(())
}

/// GET /{db}/_all_docs — query all documents.
pub async fn get_all_docs(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Query(query): Query<AllDocsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;
    let opts = query.into_options(None);
    let response = state.db.all_docs(opts).await?;
    Ok(Json(serde_json::to_value(&response).unwrap()))
}

#[derive(Deserialize, Default)]
pub struct AllDocsKeysBody {
    pub keys: Option<Vec<String>>,
}

/// POST /{db}/_all_docs — query all documents with keys in body.
pub async fn post_all_docs(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Query(query): Query<AllDocsQuery>,
    Json(body): Json<AllDocsKeysBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;
    let opts = query.into_options(body.keys);
    let response = state.db.all_docs(opts).await?;
    Ok(Json(serde_json::to_value(&response).unwrap()))
}
