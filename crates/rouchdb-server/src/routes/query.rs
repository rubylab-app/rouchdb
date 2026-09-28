use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::Deserialize;

use rouchdb::{AllDocsOptions, Database, FindOptions, IndexDefinition, SortField};
use rouchdb_core::error::RouchError;

use crate::error::AppError;
use crate::state::AppState;

/// CouchDB's `_find` limit when the request does not set one.
const DEFAULT_FIND_LIMIT: u64 = 25;

/// Bookmarks are opaque to clients; ours encode how many results of the
/// (deterministically ordered) query were already returned.
fn encode_bookmark(offset: u64) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{{\"skip\":{offset}}}"))
}

fn decode_bookmark(bookmark: &serde_json::Value) -> Option<u64> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(bookmark.as_str()?)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("skip")?.as_u64()
}

/// POST /{db}/_find — run a Mango query.
pub async fn find(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(mut body): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    state.check_db(&db)?;
    if body.get("selector").is_none() {
        return Err(AppError(RouchError::BadRequest(
            "Missing required key: selector".into(),
        )));
    }

    let offset = match body.as_object_mut().and_then(|o| o.remove("bookmark")) {
        None | Some(serde_json::Value::Null) => None,
        Some(bookmark) => match decode_bookmark(&bookmark) {
            Some(offset) => Some(offset),
            None => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_bookmark",
                        "reason": format!("Invalid bookmark value: {bookmark}"),
                    })),
                )
                    .into_response());
            }
        },
    };
    let mut opts: FindOptions = serde_json::from_value(body).map_err(|e| {
        // Keep CouchDB's own reason (e.g. `Invalid sort field: ...`) as is.
        let reason = e.to_string();
        if reason.starts_with("Invalid sort field: ") {
            AppError(RouchError::BadRequest(reason))
        } else {
            AppError(RouchError::BadRequest(format!("invalid query: {reason}")))
        }
    })?;

    // A bookmark resumes after the results already returned; `skip` applies
    // on top of it, as in CouchDB.
    let skip = offset.unwrap_or(0) + opts.skip.unwrap_or(0);
    opts.skip = Some(skip);
    opts.limit = Some(opts.limit.unwrap_or(DEFAULT_FIND_LIMIT));

    let response = state.db.find(opts).await?;
    let bookmark = if response.docs.is_empty() && offset.is_none() {
        "nil".to_string()
    } else {
        encode_bookmark(skip + response.docs.len() as u64)
    };
    Ok(Json(serde_json::json!({
        "docs": response.docs,
        "bookmark": bookmark,
    }))
    .into_response())
}

#[derive(Deserialize)]
pub struct CreateIndexBody {
    pub index: IndexFieldsBody,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub ddoc: Option<String>,
}

#[derive(Deserialize)]
pub struct IndexFieldsBody {
    pub fields: Vec<SortField>,
}

fn design_id(ddoc: &str) -> String {
    if ddoc.starts_with("_design/") {
        ddoc.to_string()
    } else {
        format!("_design/{ddoc}")
    }
}

fn missing() -> AppError {
    AppError(RouchError::NotFound("missing".into()))
}

/// A sort field must name exactly one field.
fn valid_field(field: &SortField) -> bool {
    match field {
        SortField::Simple(name) => !name.is_empty(),
        SortField::WithDirection(map) => map.len() == 1,
    }
}

/// Field name and `"asc"` / `"desc"` of a (valid) sort field.
fn field_dir(field: &SortField) -> (&str, &'static str) {
    match field {
        SortField::Simple(name) => (name, "asc"),
        SortField::WithDirection(map) => {
            let (name, dir) = map.iter().next().expect("validated sort field");
            (name, if dir == "desc" { "desc" } else { "asc" })
        }
    }
}

/// The `views` entry CouchDB stores for a Mango JSON index.
fn index_view(fields: &[SortField]) -> serde_json::Value {
    let map_fields: serde_json::Map<String, serde_json::Value> = fields
        .iter()
        .map(|f| {
            let (name, dir) = field_dir(f);
            (name.to_string(), serde_json::Value::String(dir.into()))
        })
        .collect();
    serde_json::json!({
        "map": { "fields": map_fields, "partial_filter_selector": {} },
        "reduce": "_count",
        "options": { "def": { "fields": fields } },
    })
}

/// Read the index fields back from a stored view.
fn view_fields(view: &serde_json::Value) -> Option<Vec<SortField>> {
    let fields = serde_json::from_value::<Vec<SortField>>(view["options"]["def"]["fields"].clone())
        .ok()
        .or_else(|| {
            let map = view["map"]["fields"].as_object()?;
            Some(
                map.iter()
                    .map(|(k, v)| {
                        let dir = v.as_str().unwrap_or("asc").to_string();
                        SortField::WithDirection([(k.clone(), dir)].into_iter().collect())
                    })
                    .collect(),
            )
        })?;
    (!fields.is_empty() && fields.iter().all(valid_field)).then_some(fields)
}

fn is_query_ddoc(doc: &serde_json::Value) -> bool {
    doc.get("language").and_then(|l| l.as_str()) == Some("query")
}

/// Every index stored in `language: "query"` design documents, as
/// `(ddoc id, index name, fields)`, ordered by design document then name.
async fn persisted_indexes(
    db: &Database,
) -> rouchdb::Result<Vec<(String, String, Vec<SortField>)>> {
    let rows = db
        .all_docs(AllDocsOptions {
            start_key: Some("_design/".into()),
            end_key: Some("_design0".into()),
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await?
        .rows;
    let mut out = Vec::new();
    for row in rows {
        let Some(doc) = row.doc.filter(is_query_ddoc) else {
            continue;
        };
        if let Some(views) = doc.get("views").and_then(|v| v.as_object()) {
            for (name, view) in views {
                if let Some(fields) = view_fields(view) {
                    out.push((row.id.clone(), name.clone(), fields));
                }
            }
        }
    }
    Ok(out)
}

/// Rebuild the in-memory Mango indexes from the `language: "query"` design
/// documents stored in the database (indexes created through `POST /_index`
/// or replicated from CouchDB). Returns how many indexes were built.
pub async fn restore_indexes(db: &Database) -> rouchdb::Result<usize> {
    let mut created = 0;
    for (ddoc, name, fields) in persisted_indexes(db).await? {
        let def = IndexDefinition {
            name,
            fields,
            ddoc: Some(ddoc),
        };
        if db.create_index(def).await?.result == "created" {
            created += 1;
        }
    }
    Ok(created)
}

/// Turn a failed single-document write into the matching error.
fn check_write(result: rouchdb::DocResult) -> Result<(), AppError> {
    if result.ok {
        return Ok(());
    }
    Err(AppError(match result.error.as_deref() {
        Some("conflict") => RouchError::Conflict,
        _ => RouchError::BadRequest(result.reason.unwrap_or_else(|| "write failed".into())),
    }))
}

/// Keep only the user fields of a document read back for an update.
fn body_of(doc: &rouchdb::Document) -> serde_json::Map<String, serde_json::Value> {
    let mut obj = match doc.to_json() {
        serde_json::Value::Object(obj) => obj,
        _ => serde_json::Map::new(),
    };
    obj.retain(|k, _| !k.starts_with('_'));
    obj
}

/// Store the index in its design document (creating it when needed).
async fn persist_index(
    db: &Database,
    ddoc_id: &str,
    name: &str,
    fields: &[SortField],
) -> Result<(), AppError> {
    let view = index_view(fields);
    match db.get(ddoc_id).await {
        Ok(doc) => {
            let mut body = body_of(&doc);
            if body.get("language").and_then(|l| l.as_str()) != Some("query") {
                return Err(AppError(RouchError::BadRequest(format!(
                    "{ddoc_id} is not a Mango index design document"
                ))));
            }
            let views = body.entry("views").or_insert_with(|| serde_json::json!({}));
            if !views.is_object() {
                *views = serde_json::json!({});
            }
            if views.get(name) == Some(&view) {
                return Ok(());
            }
            views[name] = view;
            let rev = doc.rev.map(|r| r.to_string()).unwrap_or_default();
            check_write(
                db.update(ddoc_id, &rev, serde_json::Value::Object(body))
                    .await?,
            )
        }
        Err(RouchError::NotFound(_)) => {
            let body = serde_json::json!({ "language": "query", "views": { name: view } });
            check_write(db.put(ddoc_id, body).await?)
        }
        Err(e) => Err(AppError(e)),
    }
}

/// POST /{db}/_index — create a Mango index.
///
/// Like CouchDB, the index is stored in a `language: "query"` design
/// document, so it survives restarts and replicates.
pub async fn create_index(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(body): Json<CreateIndexBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    state.check_db(&db)?;

    let fields = body.index.fields;
    if fields.is_empty() || !fields.iter().all(valid_field) {
        return Err(AppError(RouchError::BadRequest(
            "index.fields must be a non-empty list of field names or {field: direction}".into(),
        )));
    }
    // Same automatic name as Database::create_index; the design document
    // defaults to the index name.
    let name = body.name.filter(|n| !n.is_empty()).unwrap_or_else(|| {
        let names: Vec<&str> = fields.iter().map(|f| field_dir(f).0).collect();
        format!("idx-{}", names.join("-"))
    });
    let ddoc_id = design_id(body.ddoc.as_deref().unwrap_or(&name));

    persist_index(&state.db, &ddoc_id, &name, &fields).await?;
    let def = IndexDefinition {
        name,
        fields,
        ddoc: Some(ddoc_id.clone()),
    };
    let result = state.db.create_index(def).await?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "result": result.result,
            "id": ddoc_id,
            "name": result.name,
        })),
    ))
}

fn normalized_fields(fields: &[SortField]) -> serde_json::Value {
    fields
        .iter()
        .filter(|f| valid_field(f))
        .map(|f| {
            let (name, dir) = field_dir(f);
            serde_json::json!({ name: dir })
        })
        .collect()
}

/// GET /{db}/_index — list all indexes.
pub async fn get_indexes(
    State(state): State<AppState>,
    Path(db): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;

    // Always include the special _all_docs index
    let mut all_indexes = vec![serde_json::json!({
        "ddoc": null,
        "name": "_all_docs",
        "type": "special",
        "def": { "fields": [{"_id": "asc"}] },
    })];

    // The design documents are the source of truth; indexes created only in
    // memory through the Rust API are listed after them.
    let persisted = persisted_indexes(&state.db).await?;
    for (ddoc, name, fields) in &persisted {
        all_indexes.push(serde_json::json!({
            "ddoc": ddoc,
            "name": name,
            "type": "json",
            "partitioned": false,
            "def": { "fields": normalized_fields(fields) },
        }));
    }
    for idx in state.db.get_indexes().await {
        if persisted.iter().any(|(_, name, _)| *name == idx.name) {
            continue;
        }
        all_indexes.push(serde_json::json!({
            "ddoc": idx.ddoc,
            "name": idx.name,
            "type": "json",
            "def": { "fields": normalized_fields(&idx.def.fields) },
        }));
    }

    Ok(Json(serde_json::json!({
        "total_rows": all_indexes.len(),
        "indexes": all_indexes,
    })))
}

/// DELETE /{db}/_index/{ddoc}/json/{name} — delete an index.
///
/// The design document, type and name must all match; the index is removed
/// from its design document (and the document deleted when it was the last).
pub async fn delete_index(
    State(state): State<AppState>,
    Path((db, ddoc, itype, name)): Path<(String, String, String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;
    if itype != "json" {
        return Err(missing());
    }
    let ddoc_id = design_id(&ddoc);
    let doc = match state.db.get(&ddoc_id).await {
        Ok(doc) => doc,
        Err(RouchError::NotFound(_)) => return Err(missing()),
        Err(e) => return Err(AppError(e)),
    };
    let mut body = body_of(&doc);
    let rev = doc.rev.map(|r| r.to_string()).unwrap_or_default();
    let is_query = body.get("language").and_then(|l| l.as_str()) == Some("query");
    let views = body.get_mut("views").and_then(|v| v.as_object_mut());
    let Some(views) = views.filter(|v| is_query && v.contains_key(&name)) else {
        return Err(missing());
    };
    views.remove(&name);

    if views.is_empty() {
        check_write(state.db.remove(&ddoc_id, &rev).await?)?;
    } else {
        check_write(
            state
                .db
                .update(&ddoc_id, &rev, serde_json::Value::Object(body))
                .await?,
        )?;
    }
    // The in-memory index may be missing (e.g. never rebuilt); that is fine.
    let _ = state.db.delete_index(&name).await;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct BulkDeleteIndexBody {
    pub docids: Vec<String>,
}

/// POST /{db}/_index/_bulk_delete — delete index design documents.
pub async fn bulk_delete_indexes(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(body): Json<BulkDeleteIndexBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;

    let mut success = Vec::new();
    let mut fail = Vec::new();

    for id in body.docids {
        let ddoc_id = design_id(&id);
        let doc = match state.db.get(&ddoc_id).await {
            Ok(doc) if is_query_ddoc(&doc.to_json()) => doc,
            Ok(_) | Err(RouchError::NotFound(_)) => {
                fail.push(serde_json::json!({"id": id, "error": "not_found"}));
                continue;
            }
            Err(e) => {
                fail.push(serde_json::json!({"id": id, "error": e.to_string()}));
                continue;
            }
        };
        let rev = doc.rev.as_ref().map(|r| r.to_string()).unwrap_or_default();
        match state.db.remove(&ddoc_id, &rev).await {
            Ok(r) if r.ok => {
                if let Some(views) = doc.data.get("views").and_then(|v| v.as_object()) {
                    for name in views.keys() {
                        let _ = state.db.delete_index(name).await;
                    }
                }
                success.push(serde_json::json!({"id": id, "ok": true}));
            }
            Ok(r) => fail.push(serde_json::json!({
                "id": id,
                "error": r.error.unwrap_or_else(|| "conflict".into()),
            })),
            Err(e) => fail.push(serde_json::json!({"id": id, "error": e.to_string()})),
        }
    }

    Ok(Json(serde_json::json!({
        "success": success,
        "fail": fail,
    })))
}

/// POST /{db}/_explain — explain query execution plan.
pub async fn explain(
    State(state): State<AppState>,
    Path(db): Path<String>,
    Json(opts): Json<FindOptions>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.check_db(&db)?;
    let response = state.db.explain(opts).await;
    Ok(Json(serde_json::to_value(&response).unwrap()))
}
