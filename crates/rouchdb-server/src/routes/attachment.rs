use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use rouchdb::{AttachmentMeta, BulkDocsOptions, Document, GetAttachmentOptions, GetOptions};
use rouchdb_core::error::RouchError;

use super::document::{check_rev_format, get_or_not_found, resolve_rev};
use super::set_location;
use crate::error::{AppError, couch_error};
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct AttachmentQuery {
    pub rev: Option<String>,
}

/// Whether `rev` is not in the document's revision tree (or the document
/// does not exist at all).
async fn rev_is_unknown(state: &AppState, docid: &str, rev: &str) -> bool {
    let request = HashMap::from([(docid.to_string(), vec![rev.to_string()])]);
    state
        .db
        .adapter()
        .revs_diff(request)
        .await
        .is_ok_and(|diff| {
            diff.results
                .get(docid)
                .is_some_and(|result| result.missing.iter().any(|r| r == rev))
        })
}

/// CouchDB's answer to an attachment write on a revision that does not exist
/// (whether or not the document does): a 409 with a `not_found` error.
fn missing_rev() -> Response {
    couch_error(StatusCode::CONFLICT, "not_found", "missing_rev")
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
    check_rev_format(query.rev.as_deref())?;

    // Resolve the revision once (the requested one, or the winner) and read
    // both the metadata and the bytes from that same revision, so the
    // content type always matches the data served.
    let opts = GetOptions {
        rev: query.rev,
        ..Default::default()
    };
    let doc = get_or_not_found(&state, &docid, opts).await?;
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

    // As in CouchDB: the ETag is the digest, byte ranges are not served, and
    // the content is sandboxed so an HTML attachment cannot run scripts on
    // the database's origin.
    let mut resp = (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], data).into_response();
    let headers = resp.headers_mut();
    let digest = meta.digest.strip_prefix("md5-").unwrap_or(&meta.digest);
    if !digest.is_empty()
        && let Ok(etag) = HeaderValue::from_str(&format!("\"{digest}\""))
    {
        headers.insert(header::ETAG, etag);
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox"),
    );
    Ok(resp)
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
) -> Result<Response, AppError> {
    state.check_db(&db)?;

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    let result = match resolve_rev(query.rev, None, &headers)? {
        Some(rev) => {
            // Checked first, as in CouchDB: a malformed revision is a 400
            // even when the document does not exist.
            check_rev_format(Some(&rev))?;
            let result = state
                .db
                .put_attachment(&docid, &attname, &rev, body.to_vec(), content_type)
                .await;
            match result {
                Err(RouchError::NotFound(_) | RouchError::Conflict)
                    if rev_is_unknown(&state, &docid, &rev).await =>
                {
                    return Ok(missing_rev());
                }
                other => other?,
            }
        }
        None => {
            // Without a revision the document must not exist yet: create it
            // with the attachment in a single first revision, as CouchDB does.
            match state.db.get(&docid).await {
                Ok(_) => return Err(AppError(RouchError::Conflict)),
                Err(RouchError::NotFound(_)) => {}
                Err(e) => return Err(AppError(e)),
            }
            let attachment = AttachmentMeta::new(content_type, body.to_vec());
            let doc = Document {
                id: docid.clone(),
                rev: None,
                deleted: false,
                data: serde_json::json!({}),
                attachments: HashMap::from([(attname.clone(), attachment)]),
            };
            let mut results = state
                .db
                .bulk_docs(vec![doc], BulkDocsOptions::new())
                .await?;
            let result = results.pop().ok_or(AppError(RouchError::Conflict))?;
            super::write_result(result)?
        }
    };

    let mut resp = (
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "ok": result.ok,
            "id": result.id,
            "rev": result.rev,
        })),
    )
        .into_response();
    set_location(&mut resp, &headers, &[&db, &docid, &attname]);
    Ok(resp)
}

/// DELETE /{db}/{docid}/{attname}?rev=... — delete an attachment.
pub async fn delete_attachment(
    State(state): State<AppState>,
    Path((db, docid, attname)): Path<(String, String, String)>,
    Query(query): Query<AttachmentQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    state.check_db(&db)?;

    // Without any revision CouchDB reports a conflict.
    let rev = resolve_rev(query.rev, None, &headers)?.ok_or(AppError(RouchError::Conflict))?;
    check_rev_format(Some(&rev))?;

    let result = match state.db.remove_attachment(&docid, &attname, &rev).await {
        Err(RouchError::NotFound(_) | RouchError::Conflict)
            if rev_is_unknown(&state, &docid, &rev).await =>
        {
            return Ok(missing_rev());
        }
        Err(RouchError::NotFound(_)) => {
            return Err(AppError(RouchError::NotFound(
                "Document is missing attachment".into(),
            )));
        }
        other => other?,
    };

    Ok(axum::Json(serde_json::json!({
        "ok": result.ok,
        "id": result.id,
        "rev": result.rev,
    }))
    .into_response())
}
