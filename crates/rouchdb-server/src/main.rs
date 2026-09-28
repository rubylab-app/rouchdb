use std::process;
use std::sync::Arc;

use clap::Parser;
use rouchdb::Database;
use rouchdb_server::{AdminCredentials, parse_cors_origin};

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

    /// Host to bind to
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

    /// Largest accepted request body in bytes (documents, `_bulk_docs`
    /// batches, attachments)
    #[arg(long, value_name = "BYTES", default_value_t = rouchdb_server::DEFAULT_MAX_REQUEST_SIZE)]
    max_request_size: usize,
}

fn infer_db_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rouchdb")
        .to_string()
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let db_name = cli.db_name.unwrap_or_else(|| infer_db_name(&cli.path));

    let db = match Database::open(&cli.path, &db_name) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error opening database: {e}");
            process::exit(1);
        }
    };

    let config = rouchdb_server::ServerConfig {
        port: cli.port,
        host: cli.host,
        db_name,
        cors_origins: cli.cors_origins,
        admin: cli.admin,
        max_request_size: cli.max_request_size,
    };

    if let Err(e) = rouchdb_server::start_server(Arc::new(db), config).await {
        eprintln!("Server error: {e}");
        process::exit(1);
    }
}
