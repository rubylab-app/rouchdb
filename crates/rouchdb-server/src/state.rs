use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rouchdb::Database;
use rouchdb_core::error::RouchError;

use crate::auth::Auth;
use crate::error::AppError;

/// Shared application state for all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub db_name: String,
    /// Present when admin credentials are configured (authentication enabled).
    pub auth: Option<Arc<Auth>>,
    /// Bumped after every request that may have written, to wake up
    /// longpoll / continuous `_changes` feeds.
    pub writes: Arc<tokio::sync::watch::Sender<u64>>,
    /// Set by `DELETE /{db}` and cleared by `PUT /{db}`: while set, the
    /// database does not exist and every database route answers 404.
    pub deleted: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(db: Arc<Database>, db_name: String, auth: Option<Arc<Auth>>) -> Self {
        Self {
            db,
            db_name,
            auth,
            writes: Arc::new(tokio::sync::watch::Sender::new(0)),
            deleted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the served database currently exists (it does from startup
    /// until a `DELETE /{db}`, and again after a `PUT /{db}`).
    pub fn db_exists(&self) -> bool {
        !self.deleted.load(Ordering::SeqCst)
    }

    /// Mark the database as deleted (`true`) or existing (`false`) and return
    /// the previous state.
    pub(crate) fn set_deleted(&self, deleted: bool) -> bool {
        self.deleted.swap(deleted, Ordering::SeqCst)
    }

    /// The uuid reported by `GET /` and `GET /{db}`: 32 hex digits (like
    /// CouchDB's), the MD5 of the served database's persistent identity
    /// ([`Adapter::id`](rouchdb_core::adapter::Adapter::id)).
    ///
    /// HTTP clients identify a database by this uuid plus its name (the
    /// replication checkpoints of `rouchdb-adapter-http` and PouchDB), so it
    /// must differ between servers: for a redb file it is stable across
    /// restarts and differs between files (and for a copy of a file), and
    /// like the identity it changes when `DELETE /{db}` destroys the
    /// database.
    pub async fn uuid(&self) -> Result<String, AppError> {
        use md5::{Digest, Md5};
        let id = self.db.adapter().id().await?;
        Ok(format!("{:x}", Md5::digest(id.as_bytes())))
    }

    /// Check that `db` names the served database and that it exists, as the
    /// first step of every database-level route.
    pub fn check_db(&self, db: &str) -> Result<(), AppError> {
        if db != self.db_name || !self.db_exists() {
            return Err(db_not_found());
        }
        Ok(())
    }
}

/// CouchDB's answer for a database that does not exist.
pub fn db_not_found() -> AppError {
    AppError(RouchError::NotFound("Database does not exist.".into()))
}
