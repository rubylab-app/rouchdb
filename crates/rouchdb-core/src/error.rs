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

    /// A redb file written by rouchdb 0.4 or earlier was opened without
    /// allowing its upgrade. The file was not modified.
    ///
    /// Upgrade it once with `RedbAdapter::upgrade(path,
    /// UpgradePolicy::WithBackup(None))`, by opening it with
    /// `Database::open_with` / `RedbAdapter::open_with` and
    /// `OpenOptions::new().upgrade(UpgradePolicy::WithBackup(None))`, or
    /// with the `rouchdb migrate` command of the rouchdb CLI.
    #[error("{}", upgrade_required_message(path))]
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

fn upgrade_required_message(path: &std::path::Path) -> String {
    format!(
        "{} was written by rouchdb 0.4 or earlier and must be upgraded once before this \
         version can open it; the file was not modified. From Rust, upgrade it with \
         RedbAdapter::upgrade(path, UpgradePolicy::WithBackup(None)) or open it with \
         Database::open_with(path, name, OpenOptions::new().upgrade(UpgradePolicy::WithBackup(None))); \
         from the command line, run: rouchdb migrate {}. Each writes a verified backup first. \
         After the upgrade rouchdb 0.4 can no longer open the file",
        path.display(),
        shell_quote(&path.to_string_lossy())
    )
}

/// `s` as one POSIX shell word: unchanged if it only holds characters that
/// need no quoting, else in single quotes.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"%+,-./:=@_".contains(&b));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_required_names_the_ways_to_upgrade_with_a_quoted_path() {
        let e = RouchError::UpgradeRequired {
            path: "/data/my app's.redb".into(),
        };
        let msg = e.to_string();
        assert!(
            msg.contains(r"rouchdb migrate '/data/my app'\''s.redb'"),
            "{msg}"
        );
        assert!(msg.contains("RedbAdapter::upgrade"), "{msg}");
        assert!(msg.contains("Database::open_with"), "{msg}");
        assert!(msg.contains("the file was not modified"), "{msg}");
        let plain = RouchError::UpgradeRequired {
            path: "data/app.redb".into(),
        };
        assert!(plain.to_string().contains("rouchdb migrate data/app.redb."));
    }
}
