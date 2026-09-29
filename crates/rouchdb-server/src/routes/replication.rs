use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use rouchdb::BulkGetItem;
use rouchdb_core::error::RouchError;

use crate::error::AppError;
use crate::state::AppState;

fn bad_request(reason: &str) -> AppError {
    AppError(RouchError::BadRequest(reason.to_string()))
}

/// Parse a `{"docid": ["rev", ...]}` body (used by `_revs_diff` and `_purge`).
fn parse_rev_map(body: serde_json::Value) -> Result<HashMap<String, Vec<String>>, AppError> {
    let serde_json::Value::Object(obj) = body else {
        return Err(bad_request("Request body must be a JSON object"));
    };
    obj.into_iter()
        .map(|(id, revs)| {
            let revs = revs
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .map(|r| r.as_str().map(String::from))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| bad_request("Invalid list of revisions"))?;
            Ok((id, revs))
        })
        .collect()
}

/// Shape a document returned by `bulk_get` the way the request asked for:
/// `_revisions` only with `revs=true`, attachment bodies only with
/// `attachments=true` (stubs otherwise), as CouchDB does.
pub(crate) fn shape_doc(doc: &mut serde_json::Value, revs: bool, attachments: bool) {
    let Some(obj) = doc.as_object_mut() else {
        return;
    };
    if !revs {
        obj.remove("_revisions");
    }
    if !attachments && let Some(serde_json::Value::Object(atts)) = obj.get_mut("_attachments") {
        for meta in atts.values_mut() {
            if let Some(meta) = meta.as_object_mut()
                && meta.remove("data").is_some()
            {
                meta.insert("stub".into(), serde_json::Value::Bool(true));
            }
        }
    }
}

/// POST /{db}/_revs_diff — which of the given revisions are missing here.
pub async fn revs_diff(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;
    let revs = parse_rev_map(body)?;
    let diff = state.db.adapter().revs_diff(revs).await?;
    Ok(Json(serde_json::to_value(&diff).unwrap()))
}

#[derive(Deserialize, Default)]
pub struct BulkGetQuery {
    #[serde(default)]
    pub revs: bool,
    #[serde(default)]
    pub attachments: bool,
}

/// POST /{db}/_bulk_get — fetch several documents / revisions at once.
pub async fn bulk_get(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Query(query): Query<BulkGetQuery>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;
    let requested = body
        .get("docs")
        .and_then(|d| d.as_array())
        .ok_or_else(|| bad_request("Missing JSON list of 'docs'."))?;

    // Items without an id get a per-item error, like CouchDB; the rest are
    // fetched in one adapter call.
    let items: Vec<Option<BulkGetItem>> = requested
        .iter()
        .map(|d| {
            let id = d.get("id")?.as_str()?.to_string();
            let rev = d.get("rev").and_then(|r| r.as_str()).map(String::from);
            Some(BulkGetItem { id, rev })
        })
        .collect();
    let response = state
        .db
        .adapter()
        .bulk_get(items.iter().flatten().cloned().collect())
        .await?;
    let mut fetched = response.results.into_iter();

    let results: Vec<serde_json::Value> = items
        .iter()
        .map(|item| match item {
            Some(_) => {
                let mut result = serde_json::to_value(fetched.next()).unwrap();
                if let Some(docs) = result.get_mut("docs").and_then(|d| d.as_array_mut()) {
                    for entry in docs {
                        if entry.get("ok").is_some_and(|ok| !ok.is_null()) {
                            let mut doc = entry["ok"].take();
                            shape_doc(&mut doc, query.revs, query.attachments);
                            *entry = serde_json::json!({ "ok": doc });
                        } else {
                            let error = entry["error"].take();
                            *entry = serde_json::json!({ "error": error });
                        }
                    }
                }
                result
            }
            None => serde_json::json!({
                "id": null,
                "docs": [{"error": {
                    "id": null,
                    "rev": null,
                    "error": "bad_request",
                    "reason": "document id missed",
                }}],
            }),
        })
        .collect();

    Ok(Json(serde_json::json!({ "results": results })))
}

/// POST /{db}/_purge — permanently remove document revisions.
pub async fn purge(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    state.check_db(&db)?;
    let req = parse_rev_map(body)?;
    let ids: Vec<String> = req.keys().cloned().collect();
    let mut response = state.db.adapter().purge(req).await?;
    // CouchDB lists every requested id, with an empty list when nothing
    // was purged for it.
    for id in ids {
        response.purged.entry(id).or_default();
    }
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(&response).unwrap()),
    ))
}
