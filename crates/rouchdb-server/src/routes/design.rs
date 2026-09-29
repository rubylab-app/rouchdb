use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::document::{GetDocQuery, etag_header, get_doc, resolve_rev};
use super::set_location;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct DesignDeleteQuery {
    pub rev: Option<String>,
}

/// GET /{db}/_design/{ddoc} — get a design document, like any document
/// (every member, and the same query options and headers).
pub async fn get_design(
    state: State<AppState>,
    Path((db, ddoc)): Path<(String, String)>,
    query: Query<GetDocQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    get_doc(state, Path((db, format!("_design/{ddoc}"))), query, headers).await
}

/// PUT /{db}/_design/{ddoc} — create or update a design document.
pub async fn put_design(
    State(state): State<AppState>,
    Path((db, ddoc)): Path<(String, String)>,
    Query(query): Query<DesignDeleteQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    state.check_db(&db)?;
    let mut obj = super::json_object_body(&body)?;

    // Parse the body as a design document (which keeps every member),
    // injecting _id and the revision from `?rev`, `_rev` or `If-Match`.
    let body_rev = obj.get("_rev").and_then(|v| v.as_str()).map(String::from);
    if let Some(rev) = resolve_rev(query.rev, body_rev, &headers)? {
        obj.insert("_rev".to_string(), serde_json::Value::String(rev));
    }
    obj.insert(
        "_id".to_string(),
        serde_json::Value::String(format!("_design/{ddoc}")),
    );
    let doc_json = serde_json::Value::Object(obj);

    let design = rouchdb::DesignDocument::from_json(doc_json).map_err(|_| {
        AppError(rouchdb_core::error::RouchError::BadRequest(
            "Invalid design document".to_string(),
        ))
    })?;

    let result = super::write_result(state.db.put_design(design).await?)?;

    let mut resp = (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": result.ok,
            "id": result.id,
            "rev": result.rev,
        })),
    )
        .into_response();
    if let Some(etag) = result.rev.as_deref().and_then(etag_header) {
        resp.headers_mut().insert(header::ETAG, etag);
    }
    set_location(&mut resp, &headers, &[&db, &result.id]);
    Ok(resp)
}

/// DELETE /{db}/_design/{ddoc} — delete a design document.
pub async fn delete_design(
    State(state): State<AppState>,
    Path((db, ddoc)): Path<(String, String)>,
    Query(query): Query<DesignDeleteQuery>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;

    // Without any revision CouchDB reports a conflict.
    let rev = resolve_rev(query.rev, None, &headers)?
        .ok_or(AppError(rouchdb_core::error::RouchError::Conflict))?;

    let result = state.db.delete_design(&ddoc, &rev).await?;
    Ok(Json(serde_json::json!({
        "ok": result.ok,
        "id": result.id,
        "rev": result.rev,
    })))
}
