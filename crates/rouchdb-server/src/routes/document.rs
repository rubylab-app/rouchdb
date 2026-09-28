use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use rouchdb::{BulkGetItem, ChangesOptions, ChangesStyle, GetOptions};
use rouchdb_core::error::RouchError;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct GetDocQuery {
    pub rev: Option<String>,
    #[serde(default)]
    pub conflicts: bool,
    #[serde(default)]
    pub revs: bool,
    #[serde(default)]
    pub revs_info: bool,
    #[serde(default)]
    pub latest: bool,
    #[serde(default)]
    pub attachments: bool,
    /// `all`, or a JSON array of revisions.
    pub open_revs: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct DeleteDocQuery {
    pub rev: Option<String>,
}

fn validate_db(db: &str, state: &AppState) -> Result<(), AppError> {
    if db != state.db_name {
        return Err(AppError(rouchdb_core::error::RouchError::NotFound(
            format!("Database does not exist: {db}"),
        )));
    }
    Ok(())
}

/// Strip the quotes (and a weak `W/` prefix) from an ETag header value.
fn etag_value(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?.trim();
    let raw = raw.strip_prefix("W/").unwrap_or(raw);
    Some(raw.trim_matches('"').to_string())
}

fn etag_header(rev: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("\"{rev}\"")).ok()
}

/// Pick the revision of a write from `?rev`, the body's `_rev` and the
/// `If-Match` header, rejecting requests where they disagree (as CouchDB
/// does) instead of silently preferring one of them.
pub(crate) fn resolve_rev(
    query_rev: Option<String>,
    body_rev: Option<String>,
    headers: &HeaderMap,
) -> Result<Option<String>, AppError> {
    let rev = match (query_rev, body_rev) {
        (Some(q), Some(b)) if q != b => {
            return Err(AppError(RouchError::BadRequest(
                "Document rev from request body and query string have different values".into(),
            )));
        }
        (q, b) => q.or(b),
    };
    match (rev, etag_value(headers, header::IF_MATCH)) {
        (Some(rev), Some(etag)) if rev != etag => Err(AppError(RouchError::BadRequest(
            "Document rev and etag have different values".into(),
        ))),
        (rev, etag) => Ok(rev.or(etag)),
    }
}

/// GET /{db}/{docid} — get a document.
pub async fn get_doc(
    State(state): State<AppState>,
    Path((db, docid)): Path<(String, String)>,
    Query(query): Query<GetDocQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    validate_db(&db, &state)?;

    if let Some(open_revs) = query.open_revs.as_deref() {
        return Ok(get_open_revs(&state, &docid, open_revs, &query)
            .await?
            .into_response());
    }

    let opts = GetOptions {
        rev: query.rev,
        conflicts: query.conflicts,
        revs: query.revs,
        revs_info: query.revs_info,
        latest: query.latest,
        attachments: query.attachments,
        ..Default::default()
    };

    let doc = state.db.get_with_opts(&docid, opts).await?;
    let etag = doc.rev.as_ref().and_then(|r| etag_header(&r.to_string()));

    // The revision is the document's ETag: answer a matching If-None-Match
    // with 304 and no body.
    if let (Some(etag), Some(wanted)) = (&etag, etag_value(&headers, header::IF_NONE_MATCH))
        && etag.to_str().is_ok_and(|e| e.trim_matches('"') == wanted)
    {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag.clone())]).into_response());
    }

    let mut resp = Json(doc.to_json()).into_response();
    if let Some(etag) = etag {
        resp.headers_mut().insert(header::ETAG, etag);
    }
    Ok(resp)
}

/// `GET /{db}/{docid}?open_revs=...` — the requested leaf revisions as a JSON
/// array of `{"ok": doc}` / `{"missing": rev}` (the `Accept: application/json`
/// form; multipart responses are not supported).
async fn get_open_revs(
    state: &AppState,
    docid: &str,
    open_revs: &str,
    query: &GetDocQuery,
) -> Result<Json<serde_json::Value>, AppError> {
    let revs: Vec<String> = if open_revs == "all" {
        // Every leaf, including deleted ones, as listed by the changes feed.
        let changes = state
            .db
            .changes(ChangesOptions {
                doc_ids: Some(vec![docid.to_string()]),
                style: ChangesStyle::AllDocs,
                ..Default::default()
            })
            .await?;
        let leaves: Vec<String> = changes
            .results
            .into_iter()
            .filter(|c| c.id == docid)
            .flat_map(|c| c.changes.into_iter().map(|r| r.rev))
            .collect();
        if leaves.is_empty() {
            return Err(AppError(RouchError::NotFound("missing".into())));
        }
        leaves
    } else {
        let value: serde_json::Value = serde_json::from_str(open_revs)
            .map_err(|_| AppError(RouchError::BadRequest("invalid UTF-8 JSON".into())))?;
        value
            .as_array()
            .and_then(|a| {
                a.iter()
                    .map(|r| r.as_str().map(String::from))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| {
                AppError(RouchError::BadRequest(
                    "open_revs must be \"all\" or a JSON array of revisions".into(),
                ))
            })?
    };

    let items = revs
        .iter()
        .map(|rev| BulkGetItem {
            id: docid.to_string(),
            rev: Some(rev.clone()),
        })
        .collect();
    let response = state.db.adapter().bulk_get(items).await?;

    let mut out = Vec::with_capacity(revs.len());
    for (rev, result) in revs.iter().zip(response.results) {
        match result.docs.into_iter().next().and_then(|d| d.ok) {
            Some(mut doc) => {
                super::replication::shape_doc(&mut doc, query.revs, query.attachments);
                out.push(serde_json::json!({ "ok": doc }));
            }
            None => out.push(serde_json::json!({ "missing": rev })),
        }
    }
    Ok(Json(serde_json::Value::Array(out)))
}

/// PUT /{db}/{docid} — create or update a document.
pub async fn put_doc(
    State(state): State<AppState>,
    Path((db, docid)): Path<(String, String)>,
    Query(query): Query<DeleteDocQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    validate_db(&db, &state)?;
    let mut body = serde_json::Value::Object(super::json_object_body(&body)?);

    // Honor a `_deleted: true` body (CouchDB delete-via-PUT).
    let is_deleted = body
        .as_object()
        .and_then(|o| o.get("_deleted"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let body_rev = body.get("_rev").and_then(|v| v.as_str()).map(String::from);
    let rev = resolve_rev(query.rev, body_rev, &headers)?;

    let result = if is_deleted {
        let rev_str = rev.ok_or_else(|| {
            AppError(rouchdb_core::error::RouchError::BadRequest(
                "Missing _rev for delete".to_string(),
            ))
        })?;
        state.db.remove(&docid, &rev_str).await?
    } else if let Some(rev_str) = rev {
        // Strip _id and _rev from body data
        if let Some(obj) = body.as_object_mut() {
            obj.remove("_id");
            obj.remove("_rev");
        }
        state.db.update(&docid, &rev_str, body).await?
    } else {
        // Strip _id from body data
        if let Some(obj) = body.as_object_mut() {
            obj.remove("_id");
            obj.remove("_rev");
        }
        state.db.put(&docid, body).await?
    };

    // A failed write returns Ok(DocResult { ok: false, .. }); map it to the
    // correct HTTP status (409/404/400) instead of reporting 201 Created.
    if !result.ok {
        return Err(AppError(match result.error.as_deref() {
            Some("conflict") => rouchdb_core::error::RouchError::Conflict,
            Some("not_found") => rouchdb_core::error::RouchError::NotFound(result.id),
            _ => rouchdb_core::error::RouchError::BadRequest(
                result.reason.unwrap_or_else(|| "write failed".to_string()),
            ),
        }));
    }

    let etag = result.rev.as_deref().and_then(etag_header);
    let mut resp = (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": result.ok,
            "id": result.id,
            "rev": result.rev,
        })),
    )
        .into_response();
    if let Some(etag) = etag {
        resp.headers_mut().insert(header::ETAG, etag);
    }
    Ok(resp)
}

/// DELETE /{db}/{docid} — delete a document.
pub async fn delete_doc(
    State(state): State<AppState>,
    Path((db, docid)): Path<(String, String)>,
    Query(query): Query<DeleteDocQuery>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    // Without any revision CouchDB reports a conflict.
    let rev = resolve_rev(query.rev, None, &headers)?.ok_or(AppError(RouchError::Conflict))?;

    let result = state.db.remove(&docid, &rev).await?;
    if !result.ok {
        return Err(AppError(match result.error.as_deref() {
            Some("not_found") => rouchdb_core::error::RouchError::NotFound(
                result.reason.unwrap_or_else(|| "missing".to_string()),
            ),
            _ => rouchdb_core::error::RouchError::Conflict,
        }));
    }
    Ok(Json(serde_json::json!({
        "ok": result.ok,
        "id": result.id,
        "rev": result.rev,
    })))
}
