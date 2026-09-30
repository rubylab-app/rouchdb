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
        .and_then(|rest| {
            rest.strip_suffix("; Version=1; Max-Age=600; Path=/; HttpOnly; SameSite=Strict")
        })
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

// Q-SRV-4: sessions expire after CouchDB's default ten minutes without use.
// The clock is tokio's, paused and advanced by hand.

const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

#[tokio::test(start_paused = true)]
async fn session_survives_just_under_the_idle_timeout() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let cookie = session_cookie(&login(&app).await);

    tokio::time::advance(IDLE_TIMEOUT - Duration::from_secs(1)).await;
    let resp = get_with(&app, "/db", &[("cookie", &cookie)]).await;
    assert_eq!(resp.status, StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn session_expires_after_the_idle_timeout() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    let cookie = session_cookie(&login(&app).await);

    tokio::time::advance(IDLE_TIMEOUT).await;
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
        tokio::time::advance(Duration::from_secs(8 * 60)).await;
        let resp = get_with(&app, "/db", &[("cookie", &cookie)]).await;
        assert_eq!(resp.status, StatusCode::OK);
    }
    // 16 minutes after the login, 8 after the last use: still valid; ten
    // minutes after the last use it is gone.
    tokio::time::advance(IDLE_TIMEOUT).await;
    let resp = get_with(&app, "/db", &[("cookie", &cookie)]).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(start_paused = true)]
async fn configured_idle_timeout_applies_on_the_paused_clock() {
    let config = ServerConfig {
        session_timeout: Duration::from_secs(90),
        ..with_admin()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);
    let resp = login(&app).await;
    let set_cookie = resp.header("set-cookie").unwrap().to_string();
    assert!(set_cookie.contains("; Max-Age=90;"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_string();

    tokio::time::advance(Duration::from_secs(89)).await;
    assert_eq!(
        get_with(&app, "/db", &[("cookie", &cookie)]).await.status,
        StatusCode::OK
    );
    tokio::time::advance(Duration::from_secs(90)).await;
    assert_eq!(
        get_with(&app, "/db", &[("cookie", &cookie)]).await.status,
        StatusCode::UNAUTHORIZED
    );
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

// ─── Host header (DNS rebinding) ────────────────────────────────────────────

const REBOUND: &str = "rebind.attacker.example:5984";

fn assert_host_rejected(resp: &Resp, host: &str) {
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{host}");
    assert_eq!(
        resp.json(),
        json!({
            "error": "bad_request",
            "reason": format!("Host {host:?} is not allowed (see --allowed-host)"),
        })
    );
    assert_eq!(resp.header("access-control-allow-origin"), None, "{host}");
    assert_eq!(resp.header("set-cookie"), None, "{host}");
    assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));
}

/// On the default loopback address, a request addressed to another name
/// (a DNS-rebinding page) is a 400 before anything else runs: no route (not
/// even the public ones and Fauxton), no authentication, no CORS.
#[tokio::test]
async fn loopback_server_rejects_unknown_host_names_before_anything_else() {
    let config = ServerConfig {
        cors_origins: vec![APP.into()],
        ..with_admin()
    };
    let db = Arc::new(Database::memory(DB));
    db.put("a", json!({"v": 1})).await.unwrap();
    let app = app_with(db.clone(), &config);
    let auth = basic("admin", "s3cret");
    let wrong = basic("admin", "wrong");

    for uri in [
        "/",
        "/_utils/",
        "/_utils",
        "/_uuids",
        "/_session",
        "/db",
        "/db/a",
    ] {
        let resp = get_with(&app, uri, &[("host", REBOUND), ("authorization", &auth)]).await;
        assert_host_rejected(&resp, REBOUND);
        // Not a 401 either: the Host check runs before authentication.
        let resp = get_with(&app, uri, &[("host", REBOUND), ("authorization", &wrong)]).await;
        assert_host_rejected(&resp, REBOUND);
    }
    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/db")
        .header(header::HOST, REBOUND)
        .header(header::AUTHORIZATION, &auth)
        .body(Body::empty())
        .unwrap();
    assert_host_rejected(&send(&app, req).await, REBOUND);
    assert!(db.get("a").await.is_ok(), "the rejected DELETE did nothing");
    let req = Request::builder()
        .method(Method::POST)
        .uri("/_session")
        .header(header::HOST, REBOUND)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"name": "admin", "password": "s3cret"}"#))
        .unwrap();
    assert_host_rejected(&send(&app, req).await, REBOUND);
    // A CORS preflight too, even from an allowed origin.
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/db")
        .header(header::HOST, REBOUND)
        .header(header::ORIGIN, APP)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "DELETE")
        .body(Body::empty())
        .unwrap();
    assert_host_rejected(&send(&app, req).await, REBOUND);

    // An absolute-form request target (or an HTTP/2 authority) is checked
    // like the Host header, and every Host header must be allowed.
    let req = Request::builder()
        .uri("http://rebind.attacker.example/db")
        .header(header::AUTHORIZATION, &auth)
        .body(Body::empty())
        .unwrap();
    assert_host_rejected(&send(&app, req).await, "rebind.attacker.example");
    let req = Request::builder()
        .uri("/db")
        .header(header::HOST, "localhost:5984")
        .header(header::HOST, REBOUND)
        .header(header::AUTHORIZATION, &auth)
        .body(Body::empty())
        .unwrap();
    assert_host_rejected(&send(&app, req).await, REBOUND);

    let req = Request::builder()
        .uri("/db")
        .header(header::HOST, &b"caf\xe9"[..])
        .body(Body::empty())
        .unwrap();
    let resp = send(&app, req).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json(),
        json!({"error": "bad_request", "reason": "Malformed Host header"})
    );
}

#[tokio::test]
async fn loopback_server_answers_loopback_names_and_requests_without_host() {
    let app = app();
    for host in [
        "localhost",
        "localhost:5984",
        "LocalHost:5984",
        "127.0.0.1",
        "127.0.0.1:5984",
        "[::1]",
        "[::1]:5984",
    ] {
        let resp = get_with(&app, "/db", &[("host", host)]).await;
        assert_eq!(resp.status, StatusCode::OK, "{host}");
    }
    for host in ["127.0.0.1.nip.io", "localhost.attacker.example", "::1"] {
        let resp = get_with(&app, "/db", &[("host", host)]).await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{host}");
    }
    // HTTP/1.0 requests may omit Host; browsers never do.
    assert_eq!(get(&app, "/db").await.status, StatusCode::OK);
    let req = Request::builder()
        .uri("http://127.0.0.1:5984/db")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::OK);
}

/// The names a reverse proxy forwards are declared with `allowed_hosts`
/// (`--allowed-host`); the bind address is accepted too.
#[tokio::test]
async fn allowed_hosts_extend_the_loopback_names() {
    let config = ServerConfig {
        host: "127.0.0.2".into(),
        allowed_hosts: vec!["db.example.com".into(), "10.0.0.5".into()],
        ..config()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);
    for host in [
        "db.example.com",
        "DB.example.com:443",
        "10.0.0.5:5984",
        "127.0.0.2:5984",
        "localhost:5984",
    ] {
        let resp = get_with(&app, "/db", &[("host", host)]).await;
        assert_eq!(resp.status, StatusCode::OK, "{host}");
    }
    for host in ["example.com", "www.db.example.com", "10.0.0.6"] {
        let resp = get_with(&app, "/db", &[("host", host)]).await;
        assert_host_rejected(&resp, host);
    }
}

/// On a non-loopback address the Host header is only checked when
/// `allowed_hosts` lists names: then those, the loopback names and the bind
/// address are accepted.
#[tokio::test]
async fn non_loopback_server_checks_host_only_with_allowed_hosts() {
    let open = ServerConfig {
        host: "0.0.0.0".into(),
        ..with_admin()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &open);
    let auth = basic("admin", "s3cret");
    for host in [REBOUND, "db.example.com", "192.168.1.10:5984"] {
        let resp = get_with(&app, "/db", &[("host", host), ("authorization", &auth)]).await;
        assert_eq!(resp.status, StatusCode::OK, "{host}");
    }

    for bind in ["0.0.0.0", "::", "192.168.1.10"] {
        let checked = ServerConfig {
            host: bind.into(),
            allowed_hosts: vec!["db.example.com".into()],
            ..with_admin()
        };
        let app = app_with(Arc::new(Database::memory(DB)), &checked);
        let bind_host = if bind.contains(':') {
            format!("[{bind}]:5984")
        } else {
            format!("{bind}:5984")
        };
        for host in [
            "db.example.com:5984",
            "localhost",
            "127.0.0.1:5984",
            "[::1]:5984",
            &bind_host,
        ] {
            let resp = get_with(&app, "/db", &[("host", host), ("authorization", &auth)]).await;
            assert_eq!(resp.status, StatusCode::OK, "{bind}: {host}");
        }
        let resp = get_with(&app, "/db", &[("host", REBOUND), ("authorization", &auth)]).await;
        assert_host_rejected(&resp, REBOUND);
    }
}

/// Through a real socket (hyper parses the request): the client's Host
/// header decides.
#[tokio::test]
async fn host_check_applies_to_requests_over_tcp() {
    let addr = serve(app()).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{addr}/db"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let resp = client
        .get(format!("http://{addr}/db"))
        .header("host", REBOUND)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    assert_eq!(body["error"], "bad_request");
}

// ─── Startup checks ─────────────────────────────────────────────────────────

#[test]
fn non_loopback_address_without_admin_is_refused() {
    for host in ["0.0.0.0", "::", "[::]", "192.168.1.10", "myhost.local"] {
        let unauthenticated = ServerConfig {
            host: host.into(),
            ..config()
        };
        let err = unauthenticated.validate().unwrap_err();
        assert!(err.contains(&format!("{host:?}")), "{err}");
        assert!(err.contains("--admin user:password"), "{err}");
        assert!(err.contains("ROUCHDB_ADMIN"), "{err}");
        assert!(err.contains("--allow-unauthenticated"), "{err}");
        assert!(err.contains("ROUCHDB_ALLOW_UNAUTHENTICATED=1"), "{err}");

        let with_auth = ServerConfig {
            host: host.into(),
            ..with_admin()
        };
        assert_eq!(with_auth.validate(), Ok(()), "{host}");
        let opted_in = ServerConfig {
            host: host.into(),
            allow_unauthenticated: true,
            ..config()
        };
        assert_eq!(opted_in.validate(), Ok(()), "{host}");
    }
    for host in ["127.0.0.1", "127.0.0.2", "::1", "[::1]", "localhost"] {
        let loopback = ServerConfig {
            host: host.into(),
            ..config()
        };
        assert_eq!(loopback.validate(), Ok(()), "{host}");
    }

    let bad_host = ServerConfig {
        allowed_hosts: vec!["db.example.com:443".into()],
        ..config()
    };
    assert!(
        bad_host
            .validate()
            .unwrap_err()
            .contains("db.example.com:443")
    );
}

/// `start_server` refuses before binding. (192.0.2.1 is a documentation
/// address that no machine has, so a regression fails to bind instead of
/// listening.)
#[tokio::test]
async fn start_server_refuses_before_binding() {
    let config = ServerConfig {
        host: "192.0.2.1".into(),
        port: 0,
        ..config()
    };
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        rouchdb_server::start_server(Arc::new(Database::memory(DB)), config),
    )
    .await
    .expect("start_server returned");
    let err = result.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    assert!(err.to_string().contains("--allow-unauthenticated"), "{err}");
}

// ─── Response headers ───────────────────────────────────────────────────────

/// Every response says `X-Content-Type-Options: nosniff`, so a browser
/// never runs an attachment or a Fauxton file as another type (the
/// attachments are also sandboxed).
#[tokio::test]
async fn responses_forbid_content_sniffing() {
    let app = app();
    let resp = call(&app, Method::PUT, "/db/doc", Some(json!({}))).await;
    let rev = resp.json()["rev"].as_str().unwrap().to_string();
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/db/doc/page.txt?rev={rev}"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("<script>alert(1)</script>"))
        .unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::CREATED);

    let resp = get(&app, "/db/doc/page.txt").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.header("content-type"), Some("text/plain"));
    assert_eq!(resp.header("content-security-policy"), Some("sandbox"));
    assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));

    for uri in [
        "/_utils",
        "/_utils/",
        "/_utils/index.html",
        "/_utils/dashboard.assets/js/missing.js",
        "/",
        "/db",
        "/db/missing",
        "/nope/x/y",
    ] {
        let resp = get(&app, uri).await;
        assert_eq!(
            resp.header("x-content-type-options"),
            Some("nosniff"),
            "{uri}: {}",
            resp.status
        );
    }
    let resp = preflight(&app, "/db", EVIL).await;
    assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));
}

// ─── Secure cookie behind a trusted proxy ───────────────────────────────────

async fn login_with(app: &axum::Router, headers: &[(&str, &str)]) -> Resp {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/_session")
        .header(header::CONTENT_TYPE, "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = Body::from(r#"{"name": "admin", "password": "s3cret"}"#);
    send(app, req.body(body).unwrap()).await
}

const SESSION_ATTRS: &str = "; Version=1; Max-Age=600; Path=/; HttpOnly; SameSite=Strict";

#[tokio::test]
async fn session_cookie_is_secure_only_behind_a_trusted_https_proxy() {
    let https = [("x-forwarded-proto", "https")];

    // Without --trust-proxy the header is ignored: login over plain HTTP
    // must keep working.
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());
    for headers in [&https[..], &[]] {
        let resp = login_with(&app, headers).await;
        let set_cookie = resp.header("set-cookie").unwrap();
        assert!(set_cookie.ends_with(SESSION_ATTRS), "{set_cookie}");
    }

    let config = ServerConfig {
        trust_proxy: true,
        ..with_admin()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);
    let resp = login_with(&app, &[("x-forwarded-proto", "http")]).await;
    let set_cookie = resp.header("set-cookie").unwrap();
    assert!(set_cookie.ends_with(SESSION_ATTRS), "{set_cookie}");

    let resp = login_with(&app, &https).await;
    let set_cookie = resp.header("set-cookie").unwrap().to_string();
    assert!(
        set_cookie.ends_with(&format!("{SESSION_ATTRS}; Secure")),
        "{set_cookie}"
    );
    let cookie = set_cookie.split(';').next().unwrap().to_string();

    // The cookie refreshed on a cookie-authenticated request keeps `Secure`.
    let resp = get_with(
        &app,
        "/db",
        &[("cookie", &cookie), ("x-forwarded-proto", "https")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.header("set-cookie"),
        Some(format!("{cookie}{SESSION_ATTRS}; Secure").as_str())
    );
    let resp = get_with(&app, "/db", &[("cookie", &cookie)]).await;
    assert_eq!(
        resp.header("set-cookie"),
        Some(format!("{cookie}{SESSION_ATTRS}").as_str())
    );
}
