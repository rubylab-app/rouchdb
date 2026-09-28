use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use rouchdb::{GetAttachmentOptions, GetOptions};
use rouchdb_core::error::RouchError;

use super::document::resolve_rev;
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct AttachmentQuery {
    pub rev: Option<String>,
}

/// GET /{db}/{docid}/{attname}?rev=... — download an attachment.
///
/// The attachment name may contain `/`.
pub async fn get_attachment(
    State(state): State<AppState>,
    Path((db, docid, attname)): Path<(String, String, String)>,
    Query(query): Query<AttachmentQuery>,
) -> Result<Response, AppError> {
    state.check_db(&db)?;

    // Resolve the revision once (the requested one, or the winner) and read
    // both the metadata and the bytes from that same revision, so the
    // content type always matches the data served.
    let doc = state
        .db
        .get_with_opts(
            &docid,
            GetOptions {
                rev: query.rev,
                ..Default::default()
            },
        )
        .await?;
    let meta = doc.attachments.get(&attname).ok_or_else(|| {
        AppError(RouchError::NotFound(
            "Document is missing attachment".to_string(),
        ))
    })?;
    let data = state
        .db
        .get_attachment_with_opts(
            &docid,
            &attname,
            GetAttachmentOptions {
                rev: doc.rev.as_ref().map(|r| r.to_string()),
            },
        )
        .await?;

    // Prefer the content type stored with the attachment; fall back to a guess
    // from the name only when it is unavailable.
    let content_type = Some(meta.content_type.clone())
        .filter(|ct| !ct.is_empty())
        .unwrap_or_else(|| {
            mime_guess::from_path(&attname)
                .first_or_octet_stream()
                .to_string()
        });

    Ok((StatusCode::OK, [("content-type", content_type)], data).into_response())
}

/// PUT /{db}/{docid}/{attname}?rev=... — upload an attachment.
///
/// Without a revision (`?rev` or `If-Match`) the document is created, as in
/// CouchDB; on an existing document that is a conflict.
pub async fn put_attachment(
    State(state): State<AppState>,
    Path((db, docid, attname)): Path<(String, String, String)>,
    Query(query): Query<AttachmentQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, axum::Json<serde_json::Value>), AppError> {
    state.check_db(&db)?;

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    let rev = match resolve_rev(query.rev, None, &headers)? {
        Some(rev) => rev,
        None => match state.db.get(&docid).await {
            Ok(_) => return Err(AppError(RouchError::Conflict)),
            Err(RouchError::NotFound(_)) => {
                // The adapters attach to an existing revision, so create an
                // empty document first (the result is a 2- revision where
                // CouchDB would produce 1-).
                let created = state.db.put(&docid, serde_json::json!({})).await?;
                match created.rev {
                    Some(rev) if created.ok => rev,
                    _ => return Err(AppError(RouchError::Conflict)),
                }
            }
            Err(e) => return Err(AppError(e)),
        },
    };

    let result = state
        .db
        .put_attachment(&docid, &attname, &rev, body.to_vec(), content_type)
        .await?;

    Ok((
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "ok": result.ok,
            "id": result.id,
            "rev": result.rev,
        })),
    ))
}

/// DELETE /{db}/{docid}/{attname}?rev=... — delete an attachment.
pub async fn delete_attachment(
    State(state): State<AppState>,
    Path((db, docid, attname)): Path<(String, String, String)>,
    Query(query): Query<AttachmentQuery>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;

    // Without any revision CouchDB reports a conflict.
    let rev = resolve_rev(query.rev, None, &headers)?.ok_or(AppError(RouchError::Conflict))?;

    let result = state.db.remove_attachment(&docid, &attname, &rev).await?;

    Ok(axum::Json(serde_json::json!({
        "ok": result.ok,
        "id": result.id,
        "rev": result.rev,
    })))
}
