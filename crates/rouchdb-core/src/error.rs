use thiserror::Error;

/// All errors that RouchDB can produce.
#[derive(Debug, Error)]
pub enum RouchError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: document update conflict")]
    Conflict,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("invalid revision format: {0}")]
    InvalidRev(String),

    #[error("missing document id")]
    MissingId,

    #[error("database already exists: {0}")]
    DatabaseExists(String),

    #[error("database error: {0}")]
    DatabaseError(String),

    /// A redb file written by rouchdb 0.4 or earlier was opened without
    /// allowing its upgrade. The file was not modified.
    #[error(
        "{} was written by rouchdb 0.4 or earlier and must be upgraded before this version \
         can open it (the file was not modified). Run `rouchdb migrate {}` (it keeps a \
         backup), or open it with OpenOptions::new().upgrade(UpgradePolicy::WithBackup(None)). \
         Once upgraded, rouchdb 0.4 can no longer open the file",
        path.display(),
        path.display()
    )]
    UpgradeRequired {
        /// The database file.
        path: std::path::PathBuf,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, RouchError>;
