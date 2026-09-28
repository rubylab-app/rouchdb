use std::sync::Arc;

use rouchdb::Database;

use crate::auth::Auth;

/// Shared application state for all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub db_name: String,
    /// Present when admin credentials are configured (authentication enabled).
    pub auth: Option<Arc<Auth>>,
}
