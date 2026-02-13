use std::process;
use std::sync::Arc;

use clap::Parser;
use rouchdb::Database;

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
    };

    if let Err(e) = rouchdb_server::start_server(Arc::new(db), config).await {
        eprintln!("Server error: {e}");
        process::exit(1);
    }
}
