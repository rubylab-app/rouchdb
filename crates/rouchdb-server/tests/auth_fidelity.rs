//! Authentication as CouchDB 3.5.1 does it: wrong Basic credentials are
//! rejected on every route, anonymous requests get CouchDB's reasons, and
//! session cookies last 10 minutes of inactivity (configurable).
mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use common::*;
use rouchdb::Database;
use rouchdb_server::{AdminCredentials, ServerConfig};
use serde_json::json;

fn basic(user: &str, pass: &str) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {token}")
}

fn with_admin() -> ServerConfig {
    ServerConfig {
        admin: Some(AdminCredentials::parse("admin:s3cret").unwrap()),
        ..config()
    }
}

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Resp {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    send(app, req.body(body).unwrap()).await
}

#[track_caller]
fn assert_error(resp: &Resp, status: StatusCode, error: &str, reason: &str) {
    assert_eq!(
        resp.status,
        status,
        "{:?}",
        String::from_utf8_lossy(&resp.body)
    );
    let body = resp.json();
    assert_eq!(body["error"], error, "{body}");
    assert_eq!(body["reason"], reason, "{body}");
}

#[tokio::test]
async fn wrong_basic_credentials_are_rejected_on_every_route() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());

    for creds in [basic("nobody", "s3cret"), basic("admin", "s3creX")] {
        for uri in ["/", "/_uuids", "/_session", "/_utils/", "/_all_dbs", "/db"] {
            let resp = request(&app, Method::GET, uri, &[("authorization", &creds)], None).await;
            assert_error(
                &resp,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Name or password is incorrect.",
            );
        }
    }

    // Public routes stay public without credentials, or with an
    // Authorization header that is not Basic credentials.
    for headers in [vec![], vec![("authorization", "Bearer abc")]] {
        for uri in ["/", "/_uuids", "/_session"] {
            let resp = request(&app, Method::GET, uri, &headers, None).await;
            assert_eq!(resp.status, StatusCode::OK, "{uri} {headers:?}");
        }
    }
    let ok = basic("admin", "s3cret");
    let resp = request(&app, Method::GET, "/", &[("authorization", &ok)], None).await;
    assert_eq!(resp.status, StatusCode::OK);
}

#[tokio::test]
async fn anonymous_requests_get_couchdb_reasons() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    for uri in ["/_all_dbs", "/_active_tasks", "/_membership"] {
        assert_error(
            &get(&app, uri).await,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "You are not a server admin.",
        );
    }
    for uri in ["/db", "/db/doc", "/db/_all_docs"] {
        assert_error(
            &get(&app, uri).await,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "You are not authorized to access this db.",
        );
    }
}

#[tokio::test]
async fn session_cookie_expires_like_couchdb_and_is_refreshed() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let resp = post(
        &app,
        "/_session",
        json!({"name": "admin", "password": "s3cret"}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let set_cookie = resp.header("set-cookie").unwrap().to_string();
    assert!(set_cookie.contains("; Max-Age=600"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_string();

    // A request authenticated by the cookie extends it, as CouchDB does, so
    // an active browser session is not dropped after ten minutes.
    let resp = request(&app, Method::GET, "/db", &[("cookie", &cookie)], None).await;
    assert_eq!(resp.status, StatusCode::OK);
    let refreshed = resp.header("set-cookie").unwrap();
    assert!(refreshed.starts_with(&format!("{cookie};")), "{refreshed}");
    assert!(refreshed.contains("; Max-Age=600"), "{refreshed}");

    // Basic auth and anonymous requests get no cookie.
    let ok = basic("admin", "s3cret");
    let resp = request(&app, Method::GET, "/db", &[("authorization", &ok)], None).await;
    assert_eq!(resp.header("set-cookie"), None);
}

#[tokio::test]
async fn session_timeout_is_configurable() {
    let config = ServerConfig {
        session_timeout: Duration::from_millis(200),
        ..with_admin()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);
    let resp = post(
        &app,
        "/_session",
        json!({"name": "admin", "password": "s3cret"}),
    )
    .await;
    let set_cookie = resp.header("set-cookie").unwrap().to_string();
    // Max-Age is rounded up: 0 would tell the browser to drop the cookie.
    assert!(set_cookie.contains("; Max-Age=1"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    let with_cookie = [("cookie", cookie.as_str())];
    assert_eq!(
        request(&app, Method::GET, "/db", &with_cookie, None)
            .await
            .status,
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        request(&app, Method::GET, "/db", &with_cookie, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn logging_in_keeps_other_live_sessions() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let login = || {
        let app = app.clone();
        async move {
            let resp = post(
                &app,
                "/_session",
                json!({"name": "admin", "password": "s3cret"}),
            )
            .await;
            let set_cookie = resp.header("set-cookie").unwrap().to_string();
            set_cookie.split(';').next().unwrap().to_string()
        }
    };
    let first = login().await;
    let second = login().await;
    assert_ne!(first, second);
    for cookie in [&first, &second] {
        let resp = request(&app, Method::GET, "/db", &[("cookie", cookie)], None).await;
        assert_eq!(resp.status, StatusCode::OK);
    }
}
