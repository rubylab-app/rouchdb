use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use rouchdb::AllDocsOptions;
use rouchdb_core::error::RouchError;

use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;

/// Query-string parameters of `_all_docs`. Booleans and integers are kept as
/// strings and parsed here, so invalid values get CouchDB's
/// `query_parse_error` instead of a generic deserialization error.
#[derive(Deserialize, Default)]
pub struct AllDocsQuery {
    pub include_docs: Option<String>,
    pub startkey: Option<String>,
    pub start_key: Option<String>,
    pub endkey: Option<String>,
    pub end_key: Option<String>,
    pub key: Option<String>,
    pub keys: Option<String>,
    pub limit: Option<String>,
    pub skip: Option<String>,
    pub descending: Option<String>,
    pub inclusive_end: Option<String>,
    pub conflicts: Option<String>,
    pub update_seq: Option<String>,
}

fn query_parse_error(reason: String) -> AppError {
    AppError(RouchError::BadRequest(reason))
}

/// Parse a boolean parameter (`true` / `false`), defaulting to `default`.
fn parse_bool(raw: Option<&str>, default: bool) -> Result<bool, AppError> {
    match raw {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(other) => Err(query_parse_error(format!(
            "Invalid boolean parameter: {other:?}"
        ))),
    }
}

/// Parse a non-negative integer parameter.
fn parse_count(raw: Option<&str>) -> Result<Option<u64>, AppError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match raw.parse::<i64>() {
        Ok(n) if n < 0 => Err(query_parse_error(format!(
            "Invalid value for positive integer: {raw:?}"
        ))),
        Ok(n) => Ok(Some(n as u64)),
        Err(_) => Err(query_parse_error(format!(
            "Invalid value for integer: {raw:?}"
        ))),
    }
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
        let descending = parse_bool(self.descending.as_deref(), false)?;
        let include_docs = parse_bool(self.include_docs.as_deref(), false)?;
        let inclusive_end = parse_bool(self.inclusive_end.as_deref(), true)?;
        let conflicts = parse_bool(self.conflicts.as_deref(), false)?;
        let update_seq = parse_bool(self.update_seq.as_deref(), false)?;
        let limit = parse_count(self.limit.as_deref())?;
        let skip = parse_count(self.skip.as_deref())?.unwrap_or(0);
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
            include_docs,
            start_key: id(start),
            end_key: id(end),
            key: id(key),
            keys,
            limit,
            skip,
            descending,
            inclusive_end,
            conflicts,
            update_seq,
        }))
    }
}

async fn run_all_docs(
    state: &AppState,
    query: AllDocsQuery,
    keys: Option<Vec<String>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let descending = query.descending.as_deref() == Some("true");
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
    JsonBody(body): JsonBody<serde_json::Value>,
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
