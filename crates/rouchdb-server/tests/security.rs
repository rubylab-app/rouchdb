//! F17: CORS must be opt-in and authentication available.
mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use common::*;
use rouchdb::Database;
use rouchdb_server::{AdminCredentials, ServerConfig, parse_cors_origin};

const EVIL: &str = "https://evil.example";

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

#[test]
fn admin_credentials_parsing() {
    let creds = AdminCredentials::parse("admin:pa:ss").unwrap();
    assert_eq!(creds.username, "admin");
    assert_eq!(creds.password, "pa:ss");
    assert!(AdminCredentials::parse("admin").is_err());
    assert!(AdminCredentials::parse(":pass").is_err());
    assert!(AdminCredentials::parse("admin:").is_err());
    assert!(!format!("{creds:?}").contains("pa:ss"));
}

#[tokio::test]
async fn auth_is_off_by_default_for_local_dev() {
    let app = app();
    assert_eq!(get(&app, "/db").await.status, StatusCode::OK);
    let resp = put(&app, "/db/doc1", serde_json::json!({"a": 1})).await;
    assert_eq!(resp.status, StatusCode::CREATED);
}

#[tokio::test]
async fn admin_credentials_are_required_when_configured() {
    let db = Arc::new(Database::memory(DB));
    db.put("doc1", serde_json::json!({"secret": true}))
        .await
        .unwrap();
    let app = app_with(db.clone(), &with_admin());

    for uri in [
        "/db",
        "/db/doc1",
        "/db/_all_docs?include_docs=true",
        "/db/_changes",
        "/_all_dbs",
    ] {
        let resp = get(&app, uri).await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(resp.json()["error"], "unauthorized", "{uri}");
    }

    // A cross-site DELETE without credentials must not destroy the database.
    let resp = delete(&app, "/db").await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(db.info().await.unwrap().doc_count, 1);

    let resp = get_with(
        &app,
        "/db/doc1",
        &[("authorization", &basic("admin", "wrong"))],
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.json()["reason"], "Name or password is incorrect.");

    let resp = get_with(
        &app,
        "/db/doc1",
        &[("authorization", &basic("admin", "s3cret"))],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json()["secret"], true);
}

#[tokio::test]
async fn public_endpoints_stay_reachable_with_auth() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());

    assert_eq!(get(&app, "/").await.status, StatusCode::OK);
    assert_eq!(get(&app, "/_utils/").await.status, StatusCode::OK);
    assert_eq!(get(&app, "/_uuids").await.status, StatusCode::OK);

    let resp = get(&app, "/_session").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json()["userCtx"]["name"], serde_json::Value::Null);
    assert_eq!(resp.json()["userCtx"]["roles"], serde_json::json!([]));
}

#[tokio::test]
async fn session_cookie_login_and_logout() {
    let app = app_with(Arc::new(Database::memory(DB)), &with_admin());

    // The fixed cookie the old stub handed out must not be accepted.
    let forged = "AuthSession=YWRtaW46NjdBQkE3ODE6stHxxBdC_ZKOnMSPCkDNxVFsgeQ";
    let resp = get_with(&app, "/db", &[("cookie", forged)]).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);

    let resp = post(
        &app,
        "/_session",
        serde_json::json!({"name": "admin", "password": "nope"}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);

    let resp = post(
        &app,
        "/_session",
        serde_json::json!({"name": "admin", "password": "s3cret"}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json()["name"], "admin");
    let set_cookie = resp.header("set-cookie").unwrap().to_string();
    assert!(set_cookie.contains("HttpOnly"));
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    assert!(cookie.starts_with("AuthSession=") && cookie.len() > "AuthSession=".len());

    assert_eq!(
        get_with(&app, "/db", &[("cookie", &cookie)]).await.status,
        StatusCode::OK
    );
    let resp = get_with(&app, "/_session", &[("cookie", &cookie)]).await;
    assert_eq!(resp.json()["userCtx"]["name"], "admin");
    assert_eq!(
        resp.json()["userCtx"]["roles"],
        serde_json::json!(["_admin"])
    );

    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/_session")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::OK);
    assert_eq!(
        get_with(&app, "/db", &[("cookie", &cookie)]).await.status,
        StatusCode::UNAUTHORIZED
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
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        resp.header("set-cookie")
            .unwrap()
            .starts_with("AuthSession=")
    );

    let resp = send(&app, form("text/plain", "name=admin&password=s3cret")).await;
    assert_eq!(resp.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(resp.json()["error"], "bad_content_type");
}

#[tokio::test]
async fn cors_preflight_and_errors_work_with_auth() {
    let config = ServerConfig {
        cors_origins: vec!["http://localhost:3000".into()],
        ..with_admin()
    };
    let app = app_with(Arc::new(Database::memory(DB)), &config);

    let resp = preflight(&app, "/db", "http://localhost:3000").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("http://localhost:3000")
    );

    let resp = get_with(&app, "/db", &[("origin", "http://localhost:3000")]).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("http://localhost:3000")
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
