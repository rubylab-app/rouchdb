//! Shared helpers for the integration tests that run against a real CouchDB.
//!
//! These tests are marked `#[ignore = "requires CouchDB"]` so a plain
//! `cargo test` skips them. Start CouchDB with `docker compose up -d` and run
//! them with `bash scripts/test-couchdb.sh` (or one of them with
//! `cargo test -p rouchdb --test <file> <name> -- --ignored`).
//!
//! The server comes from `COUCHDB_URL`, default
//! `http://admin:password@localhost:15984` (see docker-compose.yml). It is
//! parsed once, in [`couchdb`]; tests take the host and credentials from
//! there instead of hardcoding them.
//!
//! Every database a test creates is named `rouchdb_test_*` and owned by a
//! [`RemoteDb`] guard, which deletes it when dropped, also when the test
//! fails. The CLI tests include this file too (`#[path]`).

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::fmt;
use std::ops::Deref;
use std::sync::OnceLock;

/// Prefix of every database the tests create, so the leftovers of a run that
/// was killed before its guards ran can be found and removed
/// (`scripts/test-couchdb.sh --sweep`).
pub const TEST_DB_PREFIX: &str = "rouchdb_test_";

const DEFAULT_COUCHDB_URL: &str = "http://admin:password@localhost:15984";

/// The CouchDB under test, from `COUCHDB_URL`.
pub struct CouchDb {
    /// Server URL with the admin credentials, without a trailing slash.
    pub url: String,
    /// The same server URL without credentials.
    pub anonymous_url: String,
    pub user: String,
    pub password: String,
}

/// The CouchDB under test (parsed from `COUCHDB_URL` on first use).
pub fn couchdb() -> &'static CouchDb {
    static COUCHDB: OnceLock<CouchDb> = OnceLock::new();
    COUCHDB.get_or_init(|| {
        let raw = std::env::var("COUCHDB_URL").unwrap_or_else(|_| DEFAULT_COUCHDB_URL.into());
        let mut url = reqwest::Url::parse(&raw)
            .unwrap_or_else(|e| panic!("COUCHDB_URL is not a valid URL ({e}): {raw}"));
        let user = percent_decode(url.username());
        let password = percent_decode(url.password().unwrap_or_default());
        assert!(
            !user.is_empty(),
            "COUCHDB_URL must include the admin credentials: http://user:password@host:port"
        );
        let url_with_credentials = url.as_str().trim_end_matches('/').to_string();
        url.set_username("").unwrap();
        url.set_password(None).unwrap();
        CouchDb {
            url: url_with_credentials,
            anonymous_url: url.as_str().trim_end_matches('/').to_string(),
            user,
            password,
        }
    })
}

/// CouchDB server URL with credentials (see [`couchdb`]).
pub fn couchdb_url() -> String {
    couchdb().url.clone()
}

/// A CouchDB database that belongs to one test. Dropping the guard deletes
/// the database (a missing one is fine), so a failing assert does not leak
/// it. It derefs to the database URL (with credentials), so `&db` works
/// wherever a `&str` URL is expected.
///
/// Set `ROUCHDB_KEEP_TEST_DBS=1` to keep the databases for debugging.
pub struct RemoteDb {
    name: String,
    url: String,
}

impl RemoteDb {
    /// Database name, e.g. `rouchdb_test_replicate_<uuid>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Database URL with credentials.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Database URL without credentials.
    pub fn anonymous_url(&self) -> String {
        format!("{}/{}", couchdb().anonymous_url, self.name)
    }
}

impl Deref for RemoteDb {
    type Target = str;

    fn deref(&self) -> &str {
        &self.url
    }
}

impl fmt::Display for RemoteDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url)
    }
}

impl Drop for RemoteDb {
    fn drop(&mut self) {
        if std::env::var_os("ROUCHDB_KEEP_TEST_DBS").is_some() {
            eprintln!("ROUCHDB_KEEP_TEST_DBS is set: keeping {}", self.name);
            return;
        }
        // Drop cannot be async, and blocking on the test's own runtime from
        // inside it panics, so delete from a thread with its own runtime.
        // That works under any #[tokio::test] flavor and outside one.
        let url = self.url.clone();
        let result = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(delete_db(&url))
        })
        .join()
        .unwrap_or_else(|_| Err("the cleanup thread panicked".into()));
        if let Err(e) = result {
            let msg = format!("could not delete test database {}: {e}", self.name);
            if std::thread::panicking() {
                // The test is already failing; a second panic would abort.
                eprintln!("{msg}");
            } else {
                panic!("{msg}");
            }
        }
    }
}

/// A new, unique database name: `rouchdb_test_[<run>_]<label>_<uuid>`, where
/// `<run>` is `ROUCHDB_TEST_RUN`, set by `scripts/test-couchdb.sh` to check
/// for leftovers of its own run only.
pub fn unique_db_name(label: &str) -> String {
    let run = std::env::var("ROUCHDB_TEST_RUN")
        .map(|run| format!("{run}_"))
        .unwrap_or_default();
    format!(
        "{TEST_DB_PREFIX}{run}{label}_{}",
        uuid::Uuid::new_v4().simple()
    )
}

/// A guard for a unique database that is *not* created, for tests where the
/// code under test creates it (or must not). Deleted on drop if it exists.
pub fn unique_remote_db(label: &str) -> RemoteDb {
    let name = unique_db_name(label);
    let url = format!("{}/{}", couchdb().url, name);
    RemoteDb { name, url }
}

/// Create a fresh CouchDB database with a unique name. It is deleted when
/// the returned guard is dropped.
pub async fn fresh_remote_db(label: &str) -> RemoteDb {
    let db = unique_remote_db(label);
    let resp = reqwest::Client::new().put(db.url()).send().await.unwrap();
    assert!(
        resp.status().is_success(),
        "Failed to create DB {}: {}",
        db.name(),
        resp.status()
    );
    db
}

/// Delete a CouchDB database now, panicking if CouchDB refuses. Cleanup does
/// not need it: [`RemoteDb`] deletes its database when dropped.
pub async fn delete_remote_db(url: &str) {
    if let Err(e) = delete_db(url).await {
        panic!("{e}");
    }
}

/// DELETE a database; a database that does not exist counts as deleted.
///
/// A 500 is retried for about a second, as `HttpAdapter::destroy` does:
/// for a moment after a database is deleted (by `delete_remote_db` before
/// the guard's drop, or by `destroy()` in the test), CouchDB can answer
/// another `DELETE` of it with a 500 `badarg` instead of a 404.
async fn delete_db(url: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let mut delay = std::time::Duration::from_millis(10);
    let mut retries = 7;
    loop {
        let resp = client
            .delete(url)
            .send()
            .await
            .map_err(|e| format!("DELETE failed: {e}"))?;
        match resp.status().as_u16() {
            200 | 202 | 404 => return Ok(()),
            500 if retries > 0 => {
                retries -= 1;
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            status => {
                return Err(format!(
                    "DELETE returned {status}: {}",
                    resp.text().await.unwrap_or_default()
                ));
            }
        }
    }
}

/// Decode the `%XX` escapes of a URL's user info.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
