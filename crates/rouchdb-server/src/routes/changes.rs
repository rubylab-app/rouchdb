use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use rouchdb_core::document::{ChangesOptions, ChangesStyle, Seq};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct ChangesQuery {
    pub since: Option<String>,
    pub limit: Option<u64>,
    #[serde(default)]
    pub descending: Option<bool>,
    #[serde(default)]
    pub include_docs: Option<bool>,
    pub style: Option<String>,
    #[serde(default)]
    pub conflicts: Option<bool>,
    pub doc_ids: Option<String>,
    pub filter: Option<String>,
    pub feed: Option<String>,
    pub timeout: Option<u64>,
    pub heartbeat: Option<u64>,
}

fn validate_db(db: &str, state: &AppState) -> Result<(), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }
    Ok(())
}

fn parse_since(since: Option<String>) -> Seq {
    match since {
        None => Seq::from(0u64),
        Some(s) => {
            if s == "now" {
                // "now" means latest — use a very large number; the adapter
                // will clamp to the actual last_seq.
                Seq::from(u64::MAX)
            } else if let Ok(n) = s.parse::<u64>() {
                Seq::from(n)
            } else {
                // CouchDB string seqs — pass through
                Seq::Str(s)
            }
        }
    }
}

/// GET /{db}/_changes — get the changes feed.
pub async fn get_changes(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Query(query): Query<ChangesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let style = match query.style.as_deref() {
        Some("all_docs") => ChangesStyle::AllDocs,
        _ => ChangesStyle::MainOnly,
    };

    let opts = ChangesOptions {
        since: parse_since(query.since),
        limit: query.limit,
        descending: query.descending.unwrap_or(false),
        include_docs: query.include_docs.unwrap_or(false),
        live: false,
        doc_ids: None,
        selector: None,
        conflicts: query.conflicts.unwrap_or(false),
        style,
    };

    let response = state.db.changes(opts).await?;
    Ok(Json(serde_json::json!({
        "results": response.results,
        "last_seq": response.last_seq,
        "pending": 0,
    })))
}

/// POST /{db}/_changes — get the changes feed with body params.
pub async fn post_changes(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Query(query): Query<ChangesQuery>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let style = match query
        .style
        .as_deref()
        .or_else(|| body.get("style").and_then(|v| v.as_str()))
    {
        Some("all_docs") => ChangesStyle::AllDocs,
        _ => ChangesStyle::MainOnly,
    };

    let doc_ids = body.get("doc_ids").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let selector = body.get("selector").cloned();

    let since = query
        .since
        .or_else(|| body.get("since").and_then(|v| v.as_str()).map(String::from));

    let opts = ChangesOptions {
        since: parse_since(since),
        limit: query
            .limit
            .or_else(|| body.get("limit").and_then(|v| v.as_u64())),
        descending: query
            .descending
            .or_else(|| body.get("descending").and_then(|v| v.as_bool()))
            .unwrap_or(false),
        include_docs: query
            .include_docs
            .or_else(|| body.get("include_docs").and_then(|v| v.as_bool()))
            .unwrap_or(false),
        live: false,
        doc_ids,
        selector,
        conflicts: query
            .conflicts
            .or_else(|| body.get("conflicts").and_then(|v| v.as_bool()))
            .unwrap_or(false),
        style,
    };

    let response = state.db.changes(opts).await?;
    Ok(Json(serde_json::json!({
        "results": response.results,
        "last_seq": response.last_seq,
        "pending": 0,
    })))
}
