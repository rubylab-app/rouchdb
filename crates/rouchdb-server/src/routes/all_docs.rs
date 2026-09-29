use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use rouchdb::{AllDocsOptions, AllDocsRow};
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

/// The string ids of a `keys` array, in order (other values never match an
/// id: they get a `not_found` row, see [`keys_response`]).
fn string_keys(keys: &[serde_json::Value]) -> Vec<String> {
    keys.iter()
        .filter_map(|k| k.as_str().map(String::from))
        .collect()
}

/// A parsed `_all_docs` request.
enum Request {
    /// A range or `key` query.
    Range(AllDocsOptions),
    /// A range that cannot match any document.
    Empty { descending: bool },
    /// A `keys` query: every requested key (any JSON value, in request
    /// order) and the options, whose `keys` are the string ones.
    Keys(AllDocsOptions, Vec<serde_json::Value>),
}

impl AllDocsQuery {
    /// Parse the request; `keys` are those of a POST body, which take
    /// precedence over `?keys=`.
    fn into_request(self, keys: Option<Vec<serde_json::Value>>) -> Result<Request, AppError> {
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
                serde_json::Value::Array(arr) => Some(arr),
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
            return Ok(Request::Empty { descending });
        }
        let id = |k: Option<KeyParam>| match k {
            Some(KeyParam::Id(s)) => Some(s),
            _ => None,
        };

        let opts = AllDocsOptions {
            include_docs,
            start_key: id(start),
            end_key: id(end),
            key: id(key),
            keys: keys.as_deref().map(string_keys),
            limit,
            skip,
            descending,
            inclusive_end,
            conflicts,
            update_seq,
        };
        Ok(match keys {
            Some(keys) => Request::Keys(opts, keys),
            None => Request::Range(opts),
        })
    }
}

/// Answer a `keys` request the way CouchDB does: one row per key, in
/// request order (reversed for `descending`), a non-string key (never a
/// document id) being a `not_found` row, with `skip` and `limit` applied to
/// that list of rows and a null `offset`.
async fn keys_response(
    state: &AppState,
    opts: AllDocsOptions,
    mut keys: Vec<serde_json::Value>,
) -> Result<serde_json::Value, AppError> {
    if opts.descending {
        keys.reverse();
    }
    // The adapter answers the string keys, in the final order; skip and
    // limit also count the rows of the other keys, so they are applied here.
    let response = state
        .db
        .all_docs(AllDocsOptions {
            keys: Some(string_keys(&keys)),
            descending: false,
            skip: 0,
            limit: None,
            ..opts.clone()
        })
        .await?;
    let mut found = response.rows.into_iter();
    let rows: Vec<serde_json::Value> = keys
        .into_iter()
        .filter_map(|key| match key {
            serde_json::Value::String(_) => {
                found.next().map(|row| row_json(row, opts.include_docs))
            }
            key => Some(serde_json::json!({"key": key, "error": "not_found"})),
        })
        .skip(opts.skip as usize)
        .take(opts.limit.map_or(usize::MAX, |l| l as usize))
        .collect();
    let mut body = serde_json::json!({
        "total_rows": response.total_rows,
        "offset": null,
        "rows": rows,
    });
    if let Some(seq) = response.update_seq {
        body["update_seq"] = serde_json::to_value(seq).unwrap_or_default();
    }
    Ok(body)
}

/// A row as CouchDB serializes it: under `include_docs`, a deleted
/// document has `"doc": null`.
fn row_json(row: AllDocsRow, include_docs: bool) -> serde_json::Value {
    let null_doc = include_docs && !row.is_error() && row.doc.is_none();
    let mut json = serde_json::to_value(row).unwrap_or_default();
    if null_doc && let Some(obj) = json.as_object_mut() {
        obj.insert("doc".into(), serde_json::Value::Null);
    }
    json
}

async fn run_all_docs(
    state: &AppState,
    query: AllDocsQuery,
    keys: Option<Vec<serde_json::Value>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let response = match query.into_request(keys)? {
        Request::Keys(opts, keys) => return Ok(Json(keys_response(state, opts, keys).await?)),
        Request::Range(opts) => state.db.all_docs(opts).await?,
        Request::Empty { descending } => {
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
        Some(serde_json::Value::Array(arr)) => Some(arr.clone()),
        Some(_) => {
            return Err(AppError(RouchError::BadRequest(
                "`keys` body member must be an array.".into(),
            )));
        }
    };
    run_all_docs(&state, query, keys).await
}
