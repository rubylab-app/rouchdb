//! Shared helpers for the in-process HTTP tests.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use rouchdb::Database;
use rouchdb_server::{ServerConfig, build_router};
use tower::ServiceExt;

/// Name of the database served by the test router.
pub const DB: &str = "db";

pub fn config() -> ServerConfig {
    ServerConfig {
        db_name: DB.to_string(),
        ..Default::default()
    }
}

/// Router over a fresh in-memory database with the default configuration.
pub fn app() -> Router {
    app_with(Arc::new(Database::memory(DB)), &config())
}

pub fn app_with(db: Arc<Database>, config: &ServerConfig) -> Router {
    build_router(db, config)
}

/// A captured response.
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "response body is not JSON ({e}): {:?}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

pub async fn send(app: &Router, req: Request<Body>) -> Resp {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

/// Send a request with an optional JSON body.
pub async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> Resp {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    send(app, builder.body(body).unwrap()).await
}

pub async fn get(app: &Router, uri: &str) -> Resp {
    call(app, Method::GET, uri, None).await
}

pub async fn post(app: &Router, uri: &str, body: serde_json::Value) -> Resp {
    call(app, Method::POST, uri, Some(body)).await
}

pub async fn put(app: &Router, uri: &str, body: serde_json::Value) -> Resp {
    call(app, Method::PUT, uri, Some(body)).await
}

pub async fn delete(app: &Router, uri: &str) -> Resp {
    call(app, Method::DELETE, uri, None).await
}

/// Serve `router` on an ephemeral loopback port and return its address.
pub async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

/// Percent-encode a JSON value for use in a query string.
pub fn q(value: serde_json::Value) -> String {
    let s = value.to_string();
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
