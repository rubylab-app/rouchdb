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

/// Log in and return the `AuthSession=<token>` cookie.
async fn login(app: &axum::Router) -> String {
    let resp = post(
        app,
        "/_session",
        json!({"name": "admin", "password": "s3cret"}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let set_cookie = resp.header("set-cookie").unwrap().to_string();
    set_cookie.split(';').next().unwrap().to_string()
}

/// CouchDB checks the method of `/` (and `/_active_tasks`) before the
/// credentials: anonymous writes there are 405, not 401.
#[tokio::test]
async fn anonymous_wrong_methods_on_the_root_are_405() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        for uri in ["/", "/_active_tasks"] {
            let resp = request(&app, method.clone(), uri, &[], Some("{}")).await;
            assert_error(
                &resp,
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "Only GET,HEAD allowed",
            );
            assert_eq!(resp.header("allow"), Some("GET,HEAD"), "{method} {uri}");
        }
    }
    // Still 401 where CouchDB authenticates first.
    for (method, uri) in [
        (Method::GET, "/_active_tasks"),
        (Method::POST, "/_all_dbs"),
        (Method::POST, "/_membership"),
        (Method::PUT, "/_utils"),
        (Method::PUT, "/_utils/index.html"),
    ] {
        let resp = request(&app, method.clone(), uri, &[], None).await;
        assert_error(
            &resp,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "You are not a server admin.",
        );
    }
}

/// Only the first `AuthSession` cookie counts, as in CouchDB: an empty one
/// makes the request anonymous even if a valid one follows.
#[tokio::test]
async fn the_first_session_cookie_is_the_session() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let cookie = login(&app).await;
    let get = |uri: &'static str, cookie: String| {
        let app = app.clone();
        async move { request(&app, Method::GET, uri, &[("cookie", &cookie)], None).await }
    };
    for anonymous in [
        format!("AuthSession=; {cookie}"),
        format!("other=1; AuthSession=; {cookie}"),
    ] {
        assert_error(
            &get("/db", anonymous.clone()).await,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "You are not authorized to access this db.",
        );
        assert_eq!(
            get("/_session", anonymous).await.json()["userCtx"]["name"],
            json!(null)
        );
    }
    let token = cookie.strip_prefix("AuthSession=").unwrap();
    for valid in [
        format!("{cookie}; AuthSession="),
        format!("{cookie}; AuthSession=YWRtaW46MTox"),
    ] {
        assert_eq!(get("/db", valid).await.status, StatusCode::OK);
    }
    // Several Cookie headers are read in order.
    let resp = request(
        &app,
        Method::GET,
        "/db",
        &[("cookie", "AuthSession="), ("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{token}");
}

/// A session cookie that cannot be one of CouchDB's (base64url of
/// `name:time:hash`) is a 400 on every route, whatever other credentials
/// come with it; a well-formed unknown one is just not a session.
#[tokio::test]
async fn malformed_session_cookies_are_400() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let basic_ok = basic("admin", "s3cret");
    for bad in [
        "garbage",
        "YWRtaW46",
        "abc%zz",
        "YWR+aW46/mJj",
        "YWRtaW46NkFCQjM4MUI",
        "a",
        // CouchDB pads the value itself: padding makes it malformed.
        "YTpiOmM=",
        "YTpiOmNk==",
    ] {
        let cookie = format!("AuthSession={bad}");
        for (method, uri) in [
            (Method::GET, "/"),
            (Method::GET, "/db"),
            (Method::GET, "/_session"),
            (Method::DELETE, "/_session"),
            (Method::GET, "/_uuids"),
            (Method::POST, "/"),
        ] {
            for headers in [
                vec![("cookie", cookie.as_str())],
                vec![
                    ("cookie", cookie.as_str()),
                    ("authorization", basic_ok.as_str()),
                ],
            ] {
                let resp = request(&app, method.clone(), uri, &headers, None).await;
                assert_error(
                    &resp,
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "Malformed AuthSession cookie. Please clear your cookies.",
                );
            }
        }
    }
    // Well-formed (name:time:hash in unpadded base64url) but unknown.
    for unknown in [
        "YWRtaW46NkFCQjM4MUI6eHg",
        "YWRtaW46enp6Onh4eA",
        "Ojo",
        "YTpiOmNk",
        "YTpiOmM",
    ] {
        let cookie = format!("AuthSession={unknown}");
        let resp = request(&app, Method::GET, "/db", &[("cookie", &cookie)], None).await;
        assert_error(
            &resp,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "You are not authorized to access this db.",
        );
        let resp = request(&app, Method::GET, "/", &[("cookie", &cookie)], None).await;
        assert_eq!(resp.status, StatusCode::OK, "{unknown}");
    }
    // Our own cookies are well-formed CouchDB-style ones.
    let cookie = login(&app).await;
    let token = cookie.strip_prefix("AuthSession=").unwrap();
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .unwrap();
    let decoded = String::from_utf8(decoded).unwrap();
    let parts: Vec<&str> = decoded.splitn(3, ':').collect();
    assert_eq!(parts.len(), 3, "{decoded}");
    assert_eq!(parts[0], "admin");
    assert!(u64::from_str_radix(parts[1], 16).is_ok(), "{decoded}");
}
