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
pub mod membership;
pub mod query;
pub mod root;
pub mod security;
pub mod session;
pub mod uuids;
pub mod views;

use axum::Router;
use axum::routing::{delete, get, post};

use crate::state::AppState;

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
        .route("/{db}/_explain", post(query::explain))
        .route("/{db}/_compact", post(compact::compact))
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
        // Attachments (before generic doc catch-all)
        .route(
            "/{db}/{docid}/{attname}",
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
