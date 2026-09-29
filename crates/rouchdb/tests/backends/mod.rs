//! Fresh databases on every local storage backend, so facade tests check
//! the in-memory and the redb adapter alike.

#![allow(dead_code)]

use rouchdb::Database;

/// The local storage backends.
pub const KINDS: [&str; 2] = ["memory", "redb"];

/// A database under test and the backend it runs on.
pub struct Backend {
    pub name: &'static str,
    pub db: Database,
    /// Keeps the redb file alive as long as the database.
    dir: tempfile::TempDir,
}

impl Backend {
    /// A fresh, empty database named `name` on backend `kind`.
    pub fn open(kind: &'static str, name: &str) -> Backend {
        let dir = tempfile::tempdir().unwrap();
        let db = match kind {
            "memory" => Database::memory(name),
            "redb" => Database::open(dir.path().join("db.redb"), name).unwrap(),
            other => panic!("unknown backend {other}"),
        };
        Backend {
            name: kind,
            db,
            dir,
        }
    }

    /// The same database with plugins added by `configure` (plugins can
    /// only be added while building a database).
    pub fn configure(self, configure: impl FnOnce(Database) -> Database) -> Backend {
        Backend {
            name: self.name,
            db: configure(self.db),
            dir: self.dir,
        }
    }
}

/// A fresh, empty database named `name` on each local backend.
pub fn backends(name: &str) -> Vec<Backend> {
    KINDS.iter().map(|kind| Backend::open(kind, name)).collect()
}

/// The ids of all-docs rows, in order.
pub fn row_ids(response: &rouchdb::AllDocsResponse) -> Vec<String> {
    response.rows.iter().map(|r| r.id.clone()).collect()
}
