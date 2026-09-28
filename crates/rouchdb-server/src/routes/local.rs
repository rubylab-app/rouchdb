use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use rouchdb_core::error::RouchError;

use super::set_location;
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct LocalQuery {
    pub rev: Option<String>,
}

fn missing() -> AppError {
    AppError(RouchError::NotFound("missing".into()))
}

/// Parse a local-document revision (`0-N`) into its counter.
fn parse_local_rev(rev: &str) -> Result<u64, AppError> {
    rev.strip_prefix("0-")
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(|| AppError(RouchError::BadRequest("Invalid rev format".into())))
}

/// GET /{db}/_local/{id} — read a local (non-replicated) document.
pub async fn get_local(
    State(state): State<AppState>,
    Path((db, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;
    let stored = match state.db.adapter().get_local(&id).await {
        Ok(doc) => doc,
        Err(RouchError::NotFound(_)) => return Err(missing()),
        Err(e) => return Err(AppError(e)),
    };

    let mut obj = match stored {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    let rev = obj
        .remove("_rev")
        .and_then(|r| r.as_str().map(String::from))
        .unwrap_or_else(|| "0-1".to_string());
    obj.remove("_id");
    let mut doc = serde_json::Map::new();
    doc.insert("_id".into(), format!("_local/{id}").into());
    doc.insert("_rev".into(), rev.into());
    doc.extend(obj);
    Ok(Json(serde_json::Value::Object(doc)))
}

/// PUT /{db}/_local/{id} — write a local document.
///
/// Like CouchDB 3, local documents are not MVCC-checked: the new revision is
/// `0-(N+1)` where `0-N` is the revision sent (or `0-1` without one).
pub async fn put_local(
    State(state): State<AppState>,
    Path((db, id)): Path<(String, String)>,
    Query(query): Query<LocalQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    state.check_db(&db)?;
    let mut obj = super::json_object_body(&body)?;

    let body_rev = obj
        .remove("_rev")
        .and_then(|r| r.as_str().map(String::from));
    let current = match query.rev.or(body_rev) {
        Some(rev) => parse_local_rev(&rev)?,
        None => 0,
    };
    let rev = format!("0-{}", current + 1);

    obj.remove("_id");
    obj.insert("_rev".into(), rev.clone().into());
    state
        .db
        .adapter()
        .put_local(&id, serde_json::Value::Object(obj))
        .await?;

    let local_id = format!("_local/{id}");
    let mut resp = (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": true,
            "id": local_id,
            "rev": rev,
        })),
    )
        .into_response();
    set_location(&mut resp, &headers, &[&db, &local_id]);
    Ok(resp)
}

/// DELETE /{db}/_local/{id} — remove a local document.
pub async fn delete_local(
    State(state): State<AppState>,
    Path((db, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;
    match state.db.adapter().remove_local(&id).await {
        Ok(()) => Ok(Json(serde_json::json!({
            "ok": true,
            "id": format!("_local/{id}"),
            "rev": "0-0",
        }))),
        Err(RouchError::NotFound(_)) => Err(missing()),
        Err(e) => Err(AppError(e)),
    }
}
