pub mod error;
pub mod routes;
pub mod state;

use std::sync::Arc;

use axum::Router;
use axum::http::{Method, header};
use rouchdb::Database;
use tower_http::cors::CorsLayer;

use crate::state::AppState;

/// Configuration for the RouchDB HTTP server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub port: u16,
    pub host: String,
    pub db_name: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 5984,
            host: "127.0.0.1".to_string(),
            db_name: "rouchdb".to_string(),
        }
    }
}

/// Build the Axum router with all routes and middleware.
pub fn build_router(db: Arc<Database>, config: &ServerConfig) -> Router {
    let state = AppState {
        db,
        db_name: config.db_name.clone(),
    };

    let cors = CorsLayer::new()
        .allow_origin(tower_http::cors::AllowOrigin::mirror_request())
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS,
        ])
        .allow_headers(tower_http::cors::AllowHeaders::mirror_request())
        .allow_credentials(true)
        .expose_headers([header::SET_COOKIE]);

    routes::build_routes(state).layer(cors)
}

/// Start the HTTP server and block until shutdown.
pub async fn start_server(db: Arc<Database>, config: ServerConfig) -> std::io::Result<()> {
    let router = build_router(db, &config);

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    println!("RouchDB server listening on http://{addr}");
    println!("Fauxton UI: http://{addr}/_utils/");
    println!("Database:   http://{addr}/{}", config.db_name);

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
