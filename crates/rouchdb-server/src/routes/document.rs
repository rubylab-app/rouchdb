use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use rouchdb::GetOptions;

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

/// PUT /{db}/{docid} — create or update a document.
pub async fn put_doc(
    State(state): State<AppState>,
    Path((db, docid)): Path<(String, String)>,
    Query(query): Query<DeleteDocQuery>,
    Json(mut body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    validate_db(&db, &state)?;

    // Get _rev from query param or body
    let rev = query.rev.or_else(|| {
        body.as_object()
            .and_then(|o| o.get("_rev"))
            .and_then(|v| v.as_str())
            .map(String::from)
    });

    let result = if let Some(rev_str) = rev {
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
    Ok(Json(serde_json::json!({
        "ok": result.ok,
        "id": result.id,
        "rev": result.rev,
    })))
}
