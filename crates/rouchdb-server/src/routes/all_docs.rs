use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use rouchdb::AllDocsOptions;
use rouchdb_core::error::RouchError;

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
    pub keys: Option<String>,
    pub limit: Option<u64>,
    pub skip: Option<u64>,
    pub descending: Option<bool>,
    pub inclusive_end: Option<bool>,
    pub conflicts: Option<bool>,
    pub update_seq: Option<bool>,
}

/// A decoded key parameter. `_all_docs` keys are document ids, and CouchDB
/// collates every non-string JSON key before all of them.
enum KeyParam {
    Id(String),
    BeforeAll,
}

fn parse_json(raw: &str) -> Result<serde_json::Value, AppError> {
    serde_json::from_str(raw)
        .map_err(|_| AppError(RouchError::BadRequest("invalid UTF-8 JSON".into())))
}

/// Parse a query-string key, which CouchDB clients send JSON-encoded (`"b"`).
fn parse_key(raw: Option<String>) -> Result<Option<KeyParam>, AppError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    Ok(Some(match parse_json(&raw)? {
        serde_json::Value::String(s) => KeyParam::Id(s),
        _ => KeyParam::BeforeAll,
    }))
}

/// Keep the string ids of a `keys` array (other values never match an id).
fn string_keys(keys: &[serde_json::Value]) -> Vec<String> {
    keys.iter()
        .filter_map(|k| k.as_str().map(String::from))
        .collect()
}

impl AllDocsQuery {
    /// Build the adapter options, or `None` when the requested range cannot
    /// match any document.
    fn into_options(self, keys: Option<Vec<String>>) -> Result<Option<AllDocsOptions>, AppError> {
        let descending = self.descending.unwrap_or(false);
        let start = parse_key(self.startkey.or(self.start_key))?;
        let end = parse_key(self.endkey.or(self.end_key))?;
        let key = parse_key(self.key)?;

        let keys = match (keys, self.keys) {
            (Some(keys), _) => Some(keys),
            (None, Some(raw)) => match parse_json(&raw)? {
                serde_json::Value::Array(arr) => Some(string_keys(&arr)),
                _ => {
                    return Err(AppError(RouchError::BadRequest(
                        "`keys` parameter must be an array.".into(),
                    )));
                }
            },
            (None, None) => None,
        };

        // A bound that sorts before every id is no bound as the lower end of
        // the range, and makes the range empty as the upper end.
        let upper = if descending { &start } else { &end };
        if keys.is_none()
            && (matches!(key, Some(KeyParam::BeforeAll))
                || matches!(upper, Some(KeyParam::BeforeAll)))
        {
            return Ok(None);
        }
        let id = |k: Option<KeyParam>| match k {
            Some(KeyParam::Id(s)) => Some(s),
            _ => None,
        };

        Ok(Some(AllDocsOptions {
            include_docs: self.include_docs.unwrap_or(false),
            start_key: id(start),
            end_key: id(end),
            key: id(key),
            keys,
            limit: self.limit,
            skip: self.skip.unwrap_or(0),
            descending,
            inclusive_end: self.inclusive_end.unwrap_or(true),
            conflicts: self.conflicts.unwrap_or(false),
            update_seq: self.update_seq.unwrap_or(false),
        }))
    }
}

async fn run_all_docs(
    state: &AppState,
    query: AllDocsQuery,
    keys: Option<Vec<String>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let descending = query.descending.unwrap_or(false);
    let response = match query.into_options(keys)? {
        Some(opts) => state.db.all_docs(opts).await?,
        None => {
            // Empty range: still report total_rows, like CouchDB.
            let mut response = state
                .db
                .all_docs(AllDocsOptions {
                    limit: Some(0),
                    ..AllDocsOptions::new()
                })
                .await?;
            response.rows.clear();
            response.offset = if descending { response.total_rows } else { 0 };
            response
        }
    };
    Ok(Json(serde_json::to_value(&response).unwrap()))
}

/// GET /{db}/_all_docs — query all documents.
pub async fn get_all_docs(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Query(query): Query<AllDocsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;
    run_all_docs(&state, query, None).await
}

/// POST /{db}/_all_docs — query all documents with keys in body.
pub async fn post_all_docs(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Query(query): Query<AllDocsQuery>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;
    let keys = match body.get("keys") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(arr)) => Some(string_keys(arr)),
        Some(_) => {
            return Err(AppError(RouchError::BadRequest(
                "`keys` body member must be an array.".into(),
            )));
        }
    };
    run_all_docs(&state, query, keys).await
}
