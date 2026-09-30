use std::process;
use std::sync::Arc;

use clap::Parser;
use rouchdb::{Database, OpenOptions, RedbAdapter, RouchError, UpgradePolicy};
use rouchdb_server::{AdminCredentials, parse_allowed_host, parse_cors_origin};

#[derive(Parser)]
#[command(
    name = "rouchdb-server",
    about = "CouchDB-compatible HTTP server for RouchDB"
)]
struct Cli {
    /// Path to the .redb file
    path: String,

    /// Port to listen on
    #[arg(short, long, default_value = "5984")]
    port: u16,

    /// Host to bind to. On an address other than loopback (e.g. 0.0.0.0)
    /// the server refuses to start without --admin, unless
    /// --allow-unauthenticated is given
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Database name (defaults to filename without extension)
    #[arg(long)]
    db_name: Option<String>,

    /// Require these admin credentials (HTTP Basic auth or a `_session`
    /// cookie) on every endpoint except `/`, `/_session`, `/_uuids` and
    /// `/_utils`. Prefer the environment variable so the password does not
    /// show up in the shell history or `ps`. Without it, authentication is
    /// disabled.
    #[arg(
        long,
        value_name = "USER:PASSWORD",
        env = "ROUCHDB_ADMIN",
        hide_env_values = true,
        value_parser = AdminCredentials::parse
    )]
    admin: Option<AdminCredentials>,

    /// Allow cross-origin (CORS) requests from this origin, e.g.
    /// `http://localhost:3000`. Repeat the flag (or comma-separate) for
    /// several origins; `*` allows any origin without credentials. CORS is
    /// disabled by default.
    #[arg(
        long = "cors-origin",
        value_name = "ORIGIN",
        env = "ROUCHDB_CORS_ORIGINS",
        value_delimiter = ',',
        value_parser = parse_cors_origin
    )]
    cors_origins: Vec<String>,

    /// Also answer requests whose `Host` header names this host (a name or
    /// an IP address, without port), e.g. the public name a reverse proxy
    /// forwards. Repeat the flag (or comma-separate) for several hosts. On a
    /// loopback --host the server only answers `localhost`, `127.0.0.1`,
    /// `[::1]` and the --host address (any port), so web pages cannot reach
    /// it through DNS rebinding; a reverse proxy that forwards its public
    /// `Host` must be listed here. On any other --host the `Host` header is
    /// only checked when this is given.
    #[arg(
        long = "allowed-host",
        value_name = "HOST",
        env = "ROUCHDB_ALLOWED_HOSTS",
        value_delimiter = ',',
        value_parser = parse_allowed_host
    )]
    allowed_hosts: Vec<String>,

    /// Serve without authentication on a non-loopback --host. Without
    /// --admin the server refuses to start on such an address, since anyone
    /// who can reach it could read, write and delete the database; only use
    /// this when every client that can reach the address is trusted
    #[arg(
        long,
        env = "ROUCHDB_ALLOW_UNAUTHENTICATED",
        value_parser = parse_switch
    )]
    allow_unauthenticated: bool,

    /// Trust the `X-Forwarded-Proto` header of a reverse proxy that
    /// terminates TLS: when it is `https`, session cookies get the `Secure`
    /// attribute. Only use it behind a proxy that sets this header
    #[arg(
        long,
        env = "ROUCHDB_TRUST_PROXY",
        value_parser = parse_switch
    )]
    trust_proxy: bool,

    /// Largest accepted request body in bytes (documents, `_bulk_docs`
    /// batches, attachments)
    #[arg(long, value_name = "BYTES", default_value_t = rouchdb_server::DEFAULT_MAX_REQUEST_SIZE)]
    max_request_size: usize,

    /// Seconds a `_session` cookie stays valid without being used (CouchDB's
    /// `[chttpd_auth] timeout`); also the cookie's `Max-Age`
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = rouchdb_server::DEFAULT_SESSION_TIMEOUT.as_secs(),
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    session_timeout: u64,

    /// Upgrade a database file written by rouchdb 0.4 or earlier before
    /// serving it, after writing a verified backup to
    /// `<path>.rouchdb-0.4.bak` (without this flag such a file is refused
    /// and left untouched). Afterwards rouchdb 0.4 cannot open the file.
    /// Needs about three times the file size of free disk space. (A file of
    /// a 0.5 development build is always upgraded, after a backup to
    /// `<path>.rouchdb-0.5-pre.bak`.)
    #[arg(long)]
    upgrade: bool,
}

/// The value of a switch set through its environment variable: `1`, `true`,
/// `yes`, `on` or `0`, `false`, `no`, `off` (any case), and empty for off.
fn parse_switch(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        _ => Err("expected 1, true, yes, on or 0, false, no, off".to_string()),
    }
}

fn infer_db_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rouchdb")
        .to_string()
}

/// Stack of the thread that starts the server (it rebuilds the Mango
/// indexes) and of the runtime's worker and blocking threads (they serve
/// the requests). Documents, selectors and index keys may be nested up to
/// `MAX_NESTING_DEPTH` levels, which parsing, matching, sorting and
/// serializing walk recursively: more than the 1 MiB main thread of Windows
/// or tokio's 2 MiB threads hold (a debug build needs about 3 MiB for a
/// selector of 1000 nested objects). The memory is reserved, not committed,
/// until used; less than the CLI's 64 MiB because a server may run many
/// threads.
const STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() {
    let cli = Cli::parse();

    std::thread::Builder::new()
        .name("rouchdb-server".into())
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(STACK_SIZE)
                .build()
                .unwrap_or_else(|e| {
                    eprintln!("Server error: {e}");
                    process::exit(1);
                });
            runtime.block_on(run(cli));
        })
        .expect("cannot start the server thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

async fn run(cli: Cli) {
    let db_name = cli.db_name.unwrap_or_else(|| infer_db_name(&cli.path));

    let config = rouchdb_server::ServerConfig {
        port: cli.port,
        host: cli.host,
        db_name: db_name.clone(),
        cors_origins: cli.cors_origins,
        admin: cli.admin,
        max_request_size: cli.max_request_size,
        session_timeout: std::time::Duration::from_secs(cli.session_timeout),
        allowed_hosts: cli.allowed_hosts,
        allow_unauthenticated: cli.allow_unauthenticated,
        trust_proxy: cli.trust_proxy,
    };
    // Before touching the database file (which `--upgrade` rewrites).
    if let Err(e) = config.validate() {
        eprintln!("Error: {e}");
        process::exit(1);
    }

    let options = if cli.upgrade {
        OpenOptions::new().upgrade(UpgradePolicy::WithBackup(None))
    } else {
        OpenOptions::new()
    };
    let db = match RedbAdapter::open_with(&cli.path, &db_name, options) {
        Ok(adapter) => {
            // The report ends with the advice that fits it (backup or not,
            // 0.4 or development-build file).
            if let Some(report) = adapter.upgrade_report() {
                eprintln!("{report}");
            }
            Database::from_adapter(Arc::new(adapter))
        }
        Err(e) => {
            eprintln!("Error opening database: {e}");
            if matches!(e, RouchError::UpgradeRequired { .. }) {
                eprintln!(
                    "hint: or restart rouchdb-server with --upgrade, which writes the same \
                     verified backup first"
                );
            }
            process::exit(1);
        }
    };

    if let Err(e) = rouchdb_server::start_server(Arc::new(db), config).await {
        eprintln!("Server error: {e}");
        process::exit(1);
    }
}
