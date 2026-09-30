pub mod auth;
pub mod error;
pub mod extract;
pub mod routes;
pub mod state;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, Method, header};
use rouchdb::Database;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};

pub use crate::auth::AdminCredentials;
use crate::auth::Auth;
pub use crate::routes::query::restore_indexes;
use crate::state::AppState;

/// Configuration for the RouchDB HTTP server.
///
/// The defaults are meant for local development: bind to loopback, no
/// authentication and no CORS, so web pages from other origins cannot read or
/// write the database through the user's browser.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub port: u16,
    pub host: String,
    pub db_name: String,
    /// Origins allowed to make cross-origin (CORS) requests, e.g.
    /// `http://localhost:3000`. Empty (the default) disables CORS. `*` allows
    /// any origin but without credentials (cookies / Authorization).
    pub cors_origins: Vec<String>,
    /// When set, every endpoint except `/`, `/_session`, `/_uuids` and the
    /// Fauxton files requires these credentials, via HTTP Basic auth or a
    /// `_session` cookie. `None` (the default) disables authentication.
    pub admin: Option<AdminCredentials>,
    /// Largest accepted request body, in bytes (documents, `_bulk_docs`
    /// batches, attachments). Larger requests get a JSON 413.
    pub max_request_size: usize,
    /// How long a `_session` cookie stays valid without being used; it is
    /// also the cookie's `Max-Age`. Defaults to CouchDB's 10 minutes.
    pub session_timeout: Duration,
}

/// Default request body limit: 64 MiB.
pub const DEFAULT_MAX_REQUEST_SIZE: usize = 64 * 1024 * 1024;

/// Default session timeout: 600 seconds, CouchDB's `[chttpd_auth] timeout`.
pub const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(600);

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 5984,
            host: "127.0.0.1".to_string(),
            db_name: "rouchdb".to_string(),
            cors_origins: Vec::new(),
            admin: None,
            max_request_size: DEFAULT_MAX_REQUEST_SIZE,
            session_timeout: DEFAULT_SESSION_TIMEOUT,
        }
    }
}

/// Validate a CORS origin (`scheme://host[:port]`, or `*`) and normalize it.
pub fn parse_cors_origin(value: &str) -> Result<String, String> {
    let origin = value.trim().trim_end_matches('/');
    if origin == "*" {
        return Ok(origin.to_string());
    }
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .ok_or_else(|| {
            format!("invalid CORS origin {value:?}: must start with http:// or https://")
        })?;
    if rest.is_empty() || rest.contains('/') || HeaderValue::from_str(origin).is_err() {
        return Err(format!(
            "invalid CORS origin {value:?}: expected scheme://host[:port]"
        ));
    }
    Ok(origin.to_string())
}

/// Build the CORS layer, or `None` when no origin is allowed.
fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }

    let cors = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS,
        ])
        .allow_headers(AllowHeaders::mirror_request())
        .expose_headers([header::CONTENT_TYPE, header::CACHE_CONTROL, header::ETAG]);

    if origins.iter().any(|o| o == "*") {
        // Browsers refuse credentials with a wildcard origin, and so do we.
        return Some(cors.allow_origin(AllowOrigin::any()));
    }

    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| parse_cors_origin(o).ok())
        .filter_map(|o| HeaderValue::from_str(&o).ok())
        .collect();
    Some(
        cors.allow_origin(AllowOrigin::list(allowed))
            .allow_credentials(true),
    )
}

/// Build the Axum router with all routes and middleware.
pub fn build_router(db: Arc<Database>, config: &ServerConfig) -> Router {
    let auth = config
        .admin
        .clone()
        .map(|admin| Arc::new(Auth::with_timeout(admin, config.session_timeout)));
    let state = AppState::new(db, config.db_name.clone(), auth);

    let routes = routes::build_routes(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            routes::changes::notify_writes,
        ))
        .layer(DefaultBodyLimit::max(config.max_request_size));
    // axum adds the `Allow` header of a 405 after the route's own layers have
    // run, so the error-shaping middleware wraps the whole route tree.
    let router = Router::new()
        .fallback_service(routes)
        .layer(axum::middleware::map_response(error::json_errors))
        .layer(axum::middleware::from_fn_with_state(
            state,
            auth::require_auth,
        ));

    // CORS goes outermost so preflights are answered before authentication
    // and error responses still carry the CORS headers.
    match cors_layer(&config.cors_origins) {
        Some(cors) => router.layer(cors),
        None => router,
    }
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Start the HTTP server and block until shutdown.
pub async fn start_server(db: Arc<Database>, config: ServerConfig) -> std::io::Result<()> {
    // Mango indexes live in memory; rebuild them from their design documents.
    if let Err(e) = restore_indexes(&db).await {
        eprintln!("WARNING: could not rebuild Mango indexes: {e}");
    }
    let router = build_router(db, &config);

    let listener =
        tokio::net::TcpListener::bind(format!("{}:{}", config.host, config.port)).await?;
    // The bound address: the port the system picked for `--port 0`.
    let addr = listener.local_addr()?;

    println!("RouchDB server listening on http://{addr}");
    println!("Fauxton UI: http://{addr}/_utils/");
    println!("Database:   http://{addr}/{}", config.db_name);

    match &config.admin {
        Some(admin) => println!("Auth:       required (admin user {:?})", admin.username),
        None if is_loopback(&config.host) => {
            println!("Auth:       disabled (set --admin or ROUCHDB_ADMIN to require credentials)")
        }
        None => eprintln!(
            "WARNING: authentication is disabled and the server listens on a non-loopback \
             address ({}); anyone who can reach it can read, write and delete the database. \
             Set --admin user:password (or ROUCHDB_ADMIN).",
            config.host
        ),
    }
    if config.cors_origins.is_empty() {
        println!("CORS:       disabled");
    } else {
        println!("CORS:       {}", config.cors_origins.join(", "));
    }

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C handler");
    println!("\nShutting down...");
}
