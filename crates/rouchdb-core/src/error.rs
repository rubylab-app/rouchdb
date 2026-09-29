use thiserror::Error;

/// All errors that RouchDB can produce.
///
/// `#[non_exhaustive]`: new kinds of errors may be added in minor releases,
/// so a `match` on it needs a catch-all arm (`_ => ...`).
#[derive(Debug, Error)]
#[non_exhaustive]
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

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, RouchError>;
