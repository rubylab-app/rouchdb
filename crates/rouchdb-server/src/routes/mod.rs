pub mod active_tasks;
pub mod all_dbs;
pub mod all_docs;
pub mod attachment;
pub mod bulk;
pub mod changes;
pub mod compact;
pub mod database;
pub mod design;
pub mod document;
pub mod fauxton;
pub mod local;
pub mod membership;
pub mod query;
pub mod replication;
pub mod root;
pub mod security;
pub mod session;
pub mod uuids;
pub mod views;

use axum::Router;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{delete, get, post};
use rouchdb_core::error::RouchError;

use crate::error::AppError;
use crate::state::AppState;

/// Parse a PUT body as a JSON object whatever its Content-Type, as CouchDB
/// does (`curl -X PUT -d '{...}'` sends `application/x-www-form-urlencoded`).
pub(crate) fn json_object_body(
    body: &[u8],
) -> Result<serde_json::Map<String, serde_json::Value>, AppError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| AppError(RouchError::BadRequest("invalid UTF-8 JSON".into())))?;
    match value {
        serde_json::Value::Object(obj) => Ok(obj),
        _ => Err(AppError(RouchError::BadRequest(
            "Document must be a JSON object".into(),
        ))),
    }
}

/// Percent-encode a path segment like CouchDB's `couch_util:url_encode`:
/// ASCII letters, digits and `_.-:` are kept, every other byte is `%XX`.
pub(crate) fn url_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The `Location` of the resource at `/{segments...}`.
///
/// Like CouchDB it is an absolute URL built from the `Host` header, with
/// `X-Forwarded-Host` / `X-Forwarded-Proto` taking precedence (for servers
/// behind a proxy); without any host it is the absolute path.
pub(crate) fn location(headers: &HeaderMap, segments: &[&str]) -> Option<HeaderValue> {
    let path: String = segments
        .iter()
        .map(|s| format!("/{}", url_encode(s)))
        .collect();
    let header_str = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let host = header_str("x-forwarded-host").or_else(|| header_str(header::HOST.as_str()));
    let value = match host {
        Some(host) => {
            let scheme = match header_str("x-forwarded-proto") {
                Some(p) if p.eq_ignore_ascii_case("https") => "https",
                _ => "http",
            };
            format!("{scheme}://{host}{path}")
        }
        None => path,
    };
    HeaderValue::from_str(&value).ok()
}

/// Add a `Location` header for the resource at `/{segments...}`.
pub(crate) fn set_location(resp: &mut Response, headers: &HeaderMap, segments: &[&str]) {
    if let Some(value) = location(headers, segments) {
        resp.headers_mut().insert(header::LOCATION, value);
    }
}

/// The 415 CouchDB answers when a request that must be `application/json`
/// (such as `POST /{db}/_compact`, which may have no body) is not; `None`
/// when the content type is right.
pub(crate) fn json_required(headers: &HeaderMap) -> Option<Response> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"));
    (!is_json).then(|| {
        crate::error::couch_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "bad_content_type",
            crate::error::JSON_CONTENT_TYPE_REQUIRED,
        )
    })
}

/// Turn a failed single-document write result into the matching error.
pub(crate) fn write_result(result: rouchdb::DocResult) -> Result<rouchdb::DocResult, AppError> {
    if result.ok {
        return Ok(result);
    }
    Err(AppError(match result.error.as_deref() {
        Some("conflict") => RouchError::Conflict,
        _ => RouchError::BadRequest(result.reason.unwrap_or_else(|| "write failed".into())),
    }))
}

/// Build the full route tree.
///
/// Order matters — specific `_`-prefixed routes must come before the
/// `/{db}/{docid}` catch-all.
pub fn build_routes(state: AppState) -> Router {
    Router::new()
        // Server-level endpoints
        .route("/", get(root::root_info))
        .route(
            "/_session",
            get(session::get_session)
                .post(session::post_session)
                .delete(session::delete_session),
        )
        .route("/_all_dbs", get(all_dbs::all_dbs))
        .route("/_uuids", get(uuids::get_uuids))
        .route("/_active_tasks", get(active_tasks::get_active_tasks))
        .route("/_membership", get(membership::get_membership))
        // Fauxton static files
        .route("/_utils", get(fauxton::fauxton_root))
        .route("/_utils/", get(fauxton::fauxton_root))
        .route("/_utils/{*path}", get(fauxton::fauxton))
        // Database-level endpoints (specific _ routes before catch-all)
        .route(
            "/{db}/_all_docs",
            get(all_docs::get_all_docs).post(all_docs::post_all_docs),
        )
        .route("/{db}/_bulk_docs", post(bulk::bulk_docs))
        .route(
            "/{db}/_changes",
            get(changes::get_changes).post(changes::post_changes),
        )
        .route("/{db}/_find", post(query::find))
        .route(
            "/{db}/_index",
            get(query::get_indexes).post(query::create_index),
        )
        .route(
            "/{db}/_index/_bulk_delete",
            post(query::bulk_delete_indexes),
        )
        .route(
            "/{db}/_index/{ddoc}/{itype}/{name}",
            delete(query::delete_index),
        )
        .route(
            "/{db}/_index/_design/{ddoc}/{itype}/{name}",
            delete(query::delete_index),
        )
        .route("/{db}/_explain", post(query::explain))
        .route("/{db}/_compact", post(compact::compact))
        // Replication protocol
        .route("/{db}/_revs_diff", post(replication::revs_diff))
        .route("/{db}/_bulk_get", post(replication::bulk_get))
        .route("/{db}/_purge", post(replication::purge))
        .route(
            "/{db}/_local/{*docid}",
            get(local::get_local)
                .put(local::put_local)
                .delete(local::delete_local),
        )
        .route(
            "/{db}/_security",
            get(security::get_security).put(security::put_security),
        )
        // Design document views and info (before generic design doc route)
        .route(
            "/{db}/_design/{ddoc}/_view/{view}",
            get(views::get_view).post(views::post_view),
        )
        .route("/{db}/_design/{ddoc}/_info", get(views::get_design_info))
        // Design documents
        .route(
            "/{db}/_design/{ddoc}",
            get(design::get_design)
                .put(design::put_design)
                .delete(design::delete_design),
        )
        // Database CRUD
        .route(
            "/{db}",
            get(database::get_db_info)
                .put(database::put_db)
                .post(database::post_doc)
                .delete(database::delete_db),
        )
        // Attachments (before generic doc catch-all); names may contain `/`
        .route(
            "/{db}/{docid}/{*attname}",
            get(attachment::get_attachment)
                .put(attachment::put_attachment)
                .delete(attachment::delete_attachment),
        )
        // Document CRUD (catch-all — must be last)
        .route(
            "/{db}/{docid}",
            get(document::get_doc)
                .put(document::put_doc)
                .delete(document::delete_doc),
        )
        .with_state(state)
}
