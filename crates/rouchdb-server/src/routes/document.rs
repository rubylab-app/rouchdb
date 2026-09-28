use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
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

/// GET /{db}/{docid} — get a document.
pub async fn get_doc(
    State(state): State<AppState>,
    Path((db, docid)): Path<(String, String)>,
    Query(query): Query<GetDocQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    if let Some(open_revs) = query.open_revs.as_deref() {
        return get_open_revs(&state, &docid, open_revs, &query).await;
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
    Ok(Json(doc.to_json()))
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
    Json(mut body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    validate_db(&db, &state)?;

    // Honor a `_deleted: true` body (CouchDB delete-via-PUT).
    let is_deleted = body
        .as_object()
        .and_then(|o| o.get("_deleted"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Get _rev from query param or body
    let rev = query.rev.or_else(|| {
        body.as_object()
            .and_then(|o| o.get("_rev"))
            .and_then(|v| v.as_str())
            .map(String::from)
    });

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

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": result.ok,
            "id": result.id,
            "rev": result.rev,
        })),
    ))
}

/// DELETE /{db}/{docid} — delete a document.
pub async fn delete_doc(
    State(state): State<AppState>,
    Path((db, docid)): Path<(String, String)>,
    Query(query): Query<DeleteDocQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_db(&db, &state)?;

    let rev = query.rev.ok_or_else(|| {
        AppError(rouchdb_core::error::RouchError::BadRequest(
            "Missing rev parameter".to_string(),
        ))
    })?;

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
