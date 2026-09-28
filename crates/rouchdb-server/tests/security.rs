//! F17: CORS must be opt-in and authentication available.
mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use common::*;
use rouchdb::Database;
use rouchdb_server::{AdminCredentials, ServerConfig, parse_cors_origin};
use serde_json::json;

const EVIL: &str = "https://evil.example";
const APP: &str = "http://localhost:3000";
/// The cookie that ends a session (logout, failed login).
const CLEAR_COOKIE: &str = "AuthSession=; Version=1; Path=/; HttpOnly; SameSite=Strict; Max-Age=0";

fn with_admin() -> ServerConfig {
    ServerConfig {
        admin: Some(AdminCredentials::parse("admin:s3cret").unwrap()),
        ..config()
    }
}

fn basic(user: &str, pass: &str) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {token}")
}

async fn get_with(app: &axum::Router, uri: &str, headers: &[(&str, &str)]) -> Resp {
    let mut req = Request::builder().uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    send(app, req.body(Body::empty()).unwrap()).await
}

/// A comma-separated header value as a set of lower-case items.
fn header_set(resp: &Resp, name: &str) -> BTreeSet<String> {
    resp.header(name)
        .unwrap_or_else(|| panic!("missing {name}: {:?}", resp.headers))
        .split(',')
        .map(|item| item.trim().to_ascii_lowercase())
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

async fn preflight(app: &axum::Router, uri: &str, origin: &str) -> Resp {
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri(uri)
        .header(header::ORIGIN, origin)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "DELETE")
        .body(Body::empty())
        .unwrap();
    send(app, req).await
}

// ─── CORS ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cors_is_disabled_by_default() {
    let app = app();

    let resp = get_with(&app, "/db/_all_docs?include_docs=true", &[("origin", EVIL)]).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.header("access-control-allow-origin"), None);
    assert_eq!(resp.header("access-control-allow-credentials"), None);

    let resp = preflight(&app, "/db", EVIL).await;
    assert_eq!(resp.header("access-control-allow-origin"), None);
    assert_eq!(resp.header("access-control-allow-methods"), None);
}

#[tokio::test]
async fn cors_allows_only_configured_origins() {
    let config = ServerConfig {
        cors_origins: vec!["http://localhost:3000".into()],
        ..config()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);

    let resp = get_with(&app, "/db", &[("origin", "http://localhost:3000")]).await;
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("http://localhost:3000")
    );
    assert_eq!(
        resp.header("access-control-allow-credentials"),
        Some("true")
    );

    let resp = preflight(&app, "/db", "http://localhost:3000").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("http://localhost:3000")
    );

    let resp = get_with(&app, "/db", &[("origin", EVIL)]).await;
    assert_eq!(resp.header("access-control-allow-origin"), None);
    let resp = preflight(&app, "/db", EVIL).await;
    assert_eq!(resp.header("access-control-allow-origin"), None);
}

#[tokio::test]
async fn cors_wildcard_never_allows_credentials() {
    let config = ServerConfig {
        cors_origins: vec!["*".into()],
        ..config()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);

    let resp = get_with(&app, "/db", &[("origin", EVIL)]).await;
    assert_eq!(resp.header("access-control-allow-origin"), Some("*"));
    assert_eq!(resp.header("access-control-allow-credentials"), None);

    let resp = preflight(&app, "/db", EVIL).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.header("access-control-allow-origin"), Some("*"));
    assert_eq!(resp.header("access-control-allow-credentials"), None);
}

/// Q-SRV-3: the preflight a browser sends before an authenticated write
/// allows PUT/DELETE and the headers it asked for, and needs no credentials.
#[tokio::test]
async fn cors_preflight_lists_methods_and_mirrors_requested_headers() {
    let config = ServerConfig {
        cors_origins: vec![APP.into()],
        ..with_admin()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);

    for (method, requested) in [
        ("PUT", "authorization, content-type"),
        ("DELETE", "x-requested-with"),
    ] {
        let req = Request::builder()
            .method(Method::OPTIONS)
            .uri("/db/doc")
            .header(header::ORIGIN, APP)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, method)
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, requested)
            .body(Body::empty())
            .unwrap();
        let resp = send(&app, req).await;
        assert_eq!(resp.status, StatusCode::OK, "{method}");
        assert!(resp.body.is_empty(), "{method}");
        assert_eq!(resp.header("access-control-allow-origin"), Some(APP));
        assert_eq!(
            resp.header("access-control-allow-credentials"),
            Some("true")
        );
        assert_eq!(
            header_set(&resp, "access-control-allow-methods"),
            set(&["get", "post", "put", "delete", "head", "options"])
        );
        assert_eq!(resp.header("access-control-allow-headers"), Some(requested));
        assert!(header_set(&resp, "vary").contains("origin"), "{method}");
    }
}

/// Q-SRV-3: actual responses (successes and auth errors) let the page read
/// the ETag and vary on the Origin so caches do not mix origins.
#[tokio::test]
async fn cors_actual_responses_expose_etag_and_vary_on_origin() {
    let config = ServerConfig {
        cors_origins: vec![APP.into()],
        ..with_admin()
    };
    let db = Arc::new(Database::memory(DB));
    let rev = db.put("a", json!({"v": 1})).await.unwrap().rev.unwrap();
    let app = app_with(db, &config);
    let expose = set(&["content-type", "cache-control", "etag"]);

    let auth = basic("admin", "s3cret");
    let resp = get_with(&app, "/db/a", &[("origin", APP), ("authorization", &auth)]).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.header("etag"), Some(format!("\"{rev}\"").as_str()));
    assert_eq!(resp.header("access-control-allow-origin"), Some(APP));
    assert_eq!(
        resp.header("access-control-allow-credentials"),
        Some("true")
    );
    assert_eq!(header_set(&resp, "access-control-expose-headers"), expose);
    assert!(header_set(&resp, "vary").contains("origin"));

    // Errors carry the CORS headers too, so the page can read the 401.
    let resp = get_with(&app, "/db/a", &[("origin", APP)]).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.header("access-control-allow-origin"), Some(APP));
    assert_eq!(header_set(&resp, "access-control-expose-headers"), expose);
    assert!(header_set(&resp, "vary").contains("origin"));

    // A response to a request without Origin still varies on it.
    let resp = get_with(&app, "/db/a", &[("authorization", &auth)]).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.header("access-control-allow-origin"), None);
    assert!(header_set(&resp, "vary").contains("origin"));
}

#[test]
fn cors_origin_validation() {
    assert_eq!(
        parse_cors_origin("http://localhost:3000/").unwrap(),
        "http://localhost:3000"
    );
    assert_eq!(parse_cors_origin("*").unwrap(), "*");
    assert!(parse_cors_origin("evil.example").is_err());
    assert!(parse_cors_origin("https://app.example/path").is_err());
    assert!(parse_cors_origin("http://").is_err());
}

// ─── Authentication ─────────────────────────────────────────────────────────
// Every route × credential combination is covered by `auth_matrix.rs`.

#[test]
fn admin_credentials_parsing() {
    let creds = AdminCredentials::parse("admin:pa:ss").unwrap();
    assert_eq!(creds.username, "admin");
    assert_eq!(creds.password, "pa:ss");
    assert!(AdminCredentials::parse("admin").is_err());
    assert!(AdminCredentials::parse(":pass").is_err());
    assert!(AdminCredentials::parse("admin:").is_err());
    assert_eq!(
        format!("{creds:?}"),
        r#"AdminCredentials { username: "admin", password: "<redacted>" }"#
    );
}

#[tokio::test]
async fn auth_is_off_by_default_for_local_dev() {
    let app = app();
    assert_eq!(get(&app, "/db").await.status, StatusCode::OK);
    let resp = put(&app, "/db/doc1", json!({"a": 1})).await;
    assert_eq!(resp.status, StatusCode::CREATED);

    // Every client is an admin ("admin party").
    for resp in [
        get(&app, "/_session").await,
        post(&app, "/_session", json!({"name": "x", "password": "y"})).await,
    ] {
        assert_eq!(resp.status, StatusCode::OK);
        let body = resp.json();
        assert_eq!(body["ok"], true, "{body}");
        assert_eq!(body["userCtx"]["roles"], json!(["_admin"]), "{body}");
    }
}

// ─── Sessions ───────────────────────────────────────────────────────────────

async fn login(app: &axum::Router) -> Resp {
    post(
        app,
        "/_session",
        json!({"name": "admin", "password": "s3cret"}),
    )
    .await
}

/// The `AuthSession=<token>` pair from a successful login, after checking
/// the whole `Set-Cookie` value.
fn session_cookie(resp: &Resp) -> String {
    assert_eq!(resp.status, StatusCode::OK);
    let cookies: Vec<_> = resp.headers.get_all("set-cookie").iter().collect();
    assert_eq!(cookies.len(), 1, "{cookies:?}");
    let set_cookie = cookies[0].to_str().unwrap();
    let token = set_cookie
        .strip_prefix("AuthSession=")
        .and_then(|rest| rest.strip_suffix("; Version=1; Path=/; HttpOnly; SameSite=Strict"))
        .unwrap_or_else(|| panic!("unexpected Set-Cookie: {set_cookie}"));
    assert!(
        !token.is_empty() && !token.contains([';', ' ', ',']),
        "{set_cookie}"
    );
    format!("AuthSession={token}")
}

fn anonymous() -> serde_json::Value {
    json!({"error": "unauthorized", "reason": "You are not authorized to access this db."})
}

fn session_info(authenticated: &str) -> serde_json::Value {
    json!({
        "ok": true,
        "userCtx": {"name": "admin", "roles": ["_admin"]},
        "info": {"authentication_handlers": ["cookie", "default"], "authenticated": authenticated},
    })
}

#[tokio::test]
async fn session_cookie_login_and_logout() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());

    // The fixed cookie the old stub handed out must not be accepted.
    let forged = "AuthSession=YWRtaW46NjdBQkE3ODE6stHxxBdC_ZKOnMSPCkDNxVFsgeQ";
    let resp = get_with(&app, "/db", &[("cookie", forged)]).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);

    let resp = login(&app).await;
    let cookie = session_cookie(&resp);
    let body = resp.json();
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["name"], "admin", "{body}");
    assert_eq!(body["roles"], json!(["_admin"]), "{body}");
    let other = session_cookie(&login(&app).await);
    assert_ne!(cookie, other, "every login gets its own token");

    assert_eq!(
        get_with(&app, "/db", &[("cookie", &cookie)]).await.status,
        StatusCode::OK
    );
    let resp = get_with(&app, "/_session", &[("cookie", &cookie)]).await;
    assert_eq!(resp.json(), session_info("cookie"));
    let resp = get_with(
        &app,
        "/_session",
        &[("authorization", &basic("admin", "s3cret"))],
    )
    .await;
    assert_eq!(resp.json(), session_info("default"));

    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/_session")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = send(&app, req).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"ok": true}));
    assert_eq!(resp.header("set-cookie"), Some(CLEAR_COOKIE));

    let resp = get_with(&app, "/db", &[("cookie", &cookie)]).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.json(), anonymous());
    // Logging out ends only that session.
    assert_eq!(
        get_with(&app, "/db", &[("cookie", &other)]).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn session_login_accepts_forms_and_rejects_other_content_types() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());

    let form = |ct: &str, body: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/_session")
            .header(header::CONTENT_TYPE, ct)
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let resp = send(
        &app,
        form(
            "application/x-www-form-urlencoded",
            "name=admin&password=s3cret",
        ),
    )
    .await;
    let cookie = session_cookie(&resp);
    assert_eq!(
        get_with(&app, "/db", &[("cookie", &cookie)]).await.status,
        StatusCode::OK
    );

    let resp = send(&app, form("text/plain", "name=admin&password=s3cret")).await;
    assert_eq!(resp.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        resp.json(),
        json!({
            "error": "bad_content_type",
            "reason": "Content-Type must be 'application/x-www-form-urlencoded' or 'application/json'",
        })
    );
    assert_eq!(resp.header("set-cookie"), None);
}

// Q-SRV-4: sessions expire after an hour without use. The clock is tokio's,
// paused and advanced by hand.

#[tokio::test(start_paused = true)]
async fn session_survives_59_idle_minutes() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let cookie = session_cookie(&login(&app).await);

    tokio::time::advance(Duration::from_secs(59 * 60)).await;
    let resp = get_with(&app, "/db", &[("cookie", &cookie)]).await;
    assert_eq!(resp.status, StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn session_expires_after_60_idle_minutes() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let cookie = session_cookie(&login(&app).await);

    tokio::time::advance(Duration::from_secs(60 * 60)).await;
    let resp = get_with(&app, "/db", &[("cookie", &cookie)]).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.json(), anonymous());
    let resp = get_with(&app, "/_session", &[("cookie", &cookie)]).await;
    assert_eq!(resp.json()["userCtx"], json!({"name": null, "roles": []}));

    // A fresh login works again.
    let cookie = session_cookie(&login(&app).await);
    assert_eq!(
        get_with(&app, "/db", &[("cookie", &cookie)]).await.status,
        StatusCode::OK
    );
}

#[tokio::test(start_paused = true)]
async fn session_activity_resets_the_idle_timer() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let cookie = session_cookie(&login(&app).await);

    for _ in 0..2 {
        tokio::time::advance(Duration::from_secs(50 * 60)).await;
        let resp = get_with(&app, "/db", &[("cookie", &cookie)]).await;
        assert_eq!(resp.status, StatusCode::OK);
    }
    // 100 minutes after the login, 50 after the last use: still valid;
    // an hour after the last use it is gone.
    tokio::time::advance(Duration::from_secs(60 * 60)).await;
    let resp = get_with(&app, "/db", &[("cookie", &cookie)]).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(start_paused = true)]
async fn new_login_keeps_other_live_sessions() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let first = session_cookie(&login(&app).await);

    tokio::time::advance(Duration::from_secs(30 * 60)).await;
    let second = session_cookie(&login(&app).await);

    for cookie in [&first, &second] {
        let resp = get_with(&app, "/db", &[("cookie", cookie)]).await;
        assert_eq!(resp.status, StatusCode::OK, "{cookie}");
    }
}

#[tokio::test]
async fn http_adapter_authenticates_with_url_credentials() {
    let db = Arc::new(Database::memory(DB));
    let addr = serve(app_with(db, &with_admin())).await;

    let anonymous = Database::http(&format!("http://{addr}/{DB}"));
    assert!(matches!(
        anonymous.info().await,
        Err(rouchdb::RouchError::Unauthorized)
    ));

    let remote = Database::http(&format!("http://admin:s3cret@{addr}/{DB}"));
    let res = remote
        .put("doc1", serde_json::json!({"x": 1}))
        .await
        .unwrap();
    assert!(res.ok);
    assert_eq!(remote.get("doc1").await.unwrap().data["x"], 1);
}
