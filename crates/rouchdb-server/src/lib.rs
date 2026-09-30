pub mod auth;
pub mod error;
pub mod extract;
pub mod host;
pub mod routes;
pub mod state;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use rouchdb::Database;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};

pub use crate::auth::AdminCredentials;
use crate::auth::Auth;
pub use crate::host::parse_allowed_host;
use crate::host::{HostAllowlist, is_loopback};
pub use crate::routes::query::restore_indexes;
use crate::state::AppState;

/// Configuration for the RouchDB HTTP server.
///
/// The defaults are meant for local development: bind to loopback, no
/// authentication and no CORS, so web pages from other origins cannot read or
/// write the database through the user's browser, and only requests
/// addressed to a loopback name (`Host` header) are served, so a web page
/// cannot reach the server through DNS rebinding either.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub port: u16,
    /// Address to listen on. On a non-loopback address the server refuses
    /// to start without `admin` unless `allow_unauthenticated` is set (see
    /// [`ServerConfig::validate`]).
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
    /// More host names (or IP addresses, without port) accepted in the
    /// `Host` header, e.g. the public name a reverse proxy forwards (checked
    /// by [`ServerConfig::validate`]). On a loopback `host` the
    /// server always checks the `Host` header and accepts `localhost`,
    /// `127.0.0.1`, `[::1]`, `host` and these names (any port), answering 400
    /// to anything else; on any other `host` it checks the header only when
    /// this list is not empty (and then also accepts the loopback names and
    /// `host`).
    pub allowed_hosts: Vec<String>,
    /// Serve on a non-loopback `host` without `admin`, where anyone who can
    /// reach the address can read, write and delete the database. Without
    /// it, [`start_server`] refuses such a configuration.
    pub allow_unauthenticated: bool,
    /// Trust the `X-Forwarded-Proto` header set by a reverse proxy: when it
    /// says `https`, session cookies get the `Secure` attribute. Only enable
    /// it behind a proxy that sets this header.
    pub trust_proxy: bool,
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
            allowed_hosts: Vec::new(),
            allow_unauthenticated: false,
            trust_proxy: false,
        }
    }
}

impl ServerConfig {
    /// Check the configuration before serving it: every `allowed_hosts`
    /// entry must be a host name or an IP address, and a non-loopback
    /// `host` needs `admin` (or an explicit `allow_unauthenticated`).
    /// [`start_server`] calls it before binding.
    pub fn validate(&self) -> Result<(), String> {
        for host in &self.allowed_hosts {
            parse_allowed_host(host)?;
        }
        if self.admin.is_none() && !self.allow_unauthenticated && !is_loopback(&self.host) {
            return Err(format!(
                "refusing to serve without authentication on the non-loopback address {:?}: \
                 anyone who can reach it could read, write and delete the database.\n\
                 Either require credentials with --admin user:password (or \
                 ROUCHDB_ADMIN=user:password), or, if every client that can reach this \
                 address is trusted, pass --allow-unauthenticated (or \
                 ROUCHDB_ALLOW_UNAUTHENTICATED=1).",
                self.host
            ));
        }
        Ok(())
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

/// Response middleware: `X-Content-Type-Options: nosniff` on every
/// response, so browsers never run an attachment or a Fauxton file as
/// another type than the one it is served with.
async fn nosniff(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Build the Axum router with all routes and middleware.
///
/// It does not call [`ServerConfig::validate`]; [`start_server`] does.
pub fn build_router(db: Arc<Database>, config: &ServerConfig) -> Router {
    let auth = config.admin.clone().map(|admin| {
        Arc::new(Auth::with_timeout(admin, config.session_timeout).trust_proxy(config.trust_proxy))
    });
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

    // CORS goes around authentication so preflights are answered before it
    // and error responses still carry the CORS headers.
    let router = match cors_layer(&config.cors_origins) {
        Some(cors) => router.layer(cors),
        None => router,
    };
    // The Host check goes around everything else: a request addressed to an
    // unknown name reaches no route (not even `/_utils`), no CORS and no
    // authentication.
    let router = match HostAllowlist::from_config(config) {
        Some(allowlist) => router.layer(axum::middleware::from_fn_with_state(
            Arc::new(allowlist),
            host::check_host,
        )),
        None => router,
    };
    router.layer(axum::middleware::map_response(nosniff))
}

/// Start the HTTP server and block until shutdown.
///
/// Fails with [`std::io::ErrorKind::InvalidInput`], before binding, when
/// [`ServerConfig::validate`] rejects the configuration.
pub async fn start_server(db: Arc<Database>, config: ServerConfig) -> std::io::Result<()> {
    config
        .validate()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
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
        // Only reached with `allow_unauthenticated` (see `validate`).
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
    match HostAllowlist::from_config(&config) {
        Some(allowlist) => println!("Hosts:      {}", allowlist.hosts().join(", ")),
        None => println!("Hosts:      any (set --allowed-host to check the Host header)"),
    }
    if config.trust_proxy {
        println!("Proxy:      trusted (X-Forwarded-Proto: https makes session cookies Secure)");
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
