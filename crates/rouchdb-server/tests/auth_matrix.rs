//! Q-SRV-1/2: with admin credentials configured, EVERY registered route and
//! method rejects anonymous requests and wrong credentials with the CouchDB
//! 401 body, and leaves the database untouched. A positive control per row
//! proves each request would have been served (and, for writes, would have
//! changed the database) with valid credentials, so the "state unchanged"
//! checks are not vacuous.
mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use common::*;
use rouchdb::{ChangesOptions, ChangesStyle, Database, GetOptions};
use rouchdb_server::{AdminCredentials, ServerConfig};
use serde_json::{Value, json};

const USER: &str = "admin";
const PASS: &str = "s3cret";
const ATT_BYTES: [u8; 5] = [0, 1, 2, 255, 254];

fn with_admin() -> ServerConfig {
    ServerConfig {
        admin: Some(AdminCredentials::parse(&format!("{USER}:{PASS}")).unwrap()),
        ..config()
    }
}

fn basic(user: &str, pass: &str) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {token}")
}

fn unauthorized(reason: &str) -> Value {
    json!({"error": "unauthorized", "reason": reason})
}

const BAD_CREDENTIALS: &str = "Name or password is incorrect.";
const NOT_AUTHORIZED_DB: &str = "You are not authorized to access this db.";

async fn request(
    app: &Router,
    method: Method,
    uri: &str,
    headers: &[(&str, String)],
    body: Option<(&str, Vec<u8>)>,
) -> Resp {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, v.as_str());
    }
    let body = match body {
        Some((ct, bytes)) => {
            req = req.header(header::CONTENT_TYPE, ct);
            Body::from(bytes)
        }
        None => Body::empty(),
    };
    send(app, req.body(body).unwrap()).await
}

fn admin_headers() -> Vec<(&'static str, String)> {
    vec![("authorization", basic(USER, PASS))]
}

async fn admin_json(app: &Router, method: Method, uri: &str, body: Value) -> Value {
    let resp = request(
        app,
        method,
        uri,
        &admin_headers(),
        Some(("application/json", body.to_string().into_bytes())),
    )
    .await;
    assert!(
        resp.status.is_success(),
        "{uri}: {} {:?}",
        resp.status,
        String::from_utf8_lossy(&resp.body)
    );
    resp.json()
}

/// Log in through `POST /_session` and return the session token.
async fn login(app: &Router) -> String {
    let resp = request(
        app,
        Method::POST,
        "/_session",
        &[],
        Some((
            "application/json",
            json!({"name": USER, "password": PASS})
                .to_string()
                .into_bytes(),
        )),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let set_cookie = resp.header("set-cookie").unwrap();
    let token = set_cookie
        .strip_prefix("AuthSession=")
        .and_then(|rest| rest.split(';').next())
        .unwrap();
    assert!(!token.is_empty(), "{set_cookie}");
    token.to_string()
}

/// Revisions of the seeded documents, used to build requests that would
/// succeed (and write) if authentication were bypassed.
struct Fixture {
    db: Arc<Database>,
    app: Router,
    a_rev1: String,
    a_rev2: String,
    ddoc_rev: String,
    att_rev: String,
    token: String,
}

/// A database with documents, a design document with a view, an attachment,
/// a `_local` document, a Mango index and a security document.
async fn fixture() -> Fixture {
    let db = Arc::new(Database::memory(DB));
    let app = app_with(db.clone(), &with_admin());

    let a_rev1 = admin_json(&app, Method::PUT, "/db/a", json!({"n": 1})).await["rev"]
        .as_str()
        .unwrap()
        .to_string();
    let a_rev2 =
        admin_json(&app, Method::PUT, "/db/a", json!({"_rev": a_rev1, "n": 2})).await["rev"]
            .as_str()
            .unwrap()
            .to_string();
    admin_json(&app, Method::PUT, "/db/b", json!({"n": 3})).await;
    let ddoc_rev = admin_json(
        &app,
        Method::PUT,
        "/db/_design/d",
        json!({"views": {"v": {"map": "function(doc) { emit(doc._id, null); }"}}}),
    )
    .await["rev"]
        .as_str()
        .unwrap()
        .to_string();
    let att_doc_rev = admin_json(&app, Method::PUT, "/db/att", json!({})).await["rev"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = request(
        &app,
        Method::PUT,
        &format!("/db/att/f.bin?rev={att_doc_rev}"),
        &admin_headers(),
        Some(("application/octet-stream", ATT_BYTES.to_vec())),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let att_rev = resp.json()["rev"].as_str().unwrap().to_string();
    admin_json(&app, Method::PUT, "/db/_local/ck", json!({"v": 1})).await;
    let created = admin_json(
        &app,
        Method::POST,
        "/db/_index",
        json!({"index": {"fields": ["n"]}, "name": "by-n", "ddoc": "idx"}),
    )
    .await;
    assert_eq!(created["result"], "created");
    admin_json(
        &app,
        Method::PUT,
        "/db/_security",
        json!({"admins": {"names": ["boss"], "roles": []}, "members": {"names": [], "roles": ["r"]}}),
    )
    .await;

    let token = login(&app).await;
    Fixture {
        db,
        app,
        a_rev1,
        a_rev2,
        ddoc_rev,
        att_rev,
        token,
    }
}

/// Everything a request could have changed, read straight from the database.
#[derive(Debug, PartialEq)]
struct Snapshot {
    doc_count: u64,
    doc_del_count: u64,
    update_seq: u64,
    /// `(seq, id, leaf revs, deleted)` for every document.
    changes: Vec<(u64, String, Vec<String>, bool)>,
    /// Body of the non-leaf revision `1-` of `a` (gone after compaction).
    a_rev1_body: Option<Value>,
    local: Option<Value>,
    security: Value,
    indexes: Vec<(Option<String>, String)>,
    attachment: Option<Vec<u8>>,
}

async fn snapshot(fx: &Fixture) -> Snapshot {
    let db = &fx.db;
    let info = db.info().await.unwrap();
    let changes = db
        .changes(ChangesOptions {
            style: ChangesStyle::AllDocs,
            ..Default::default()
        })
        .await
        .unwrap()
        .results
        .into_iter()
        .map(|c| {
            let mut revs: Vec<String> = c.changes.into_iter().map(|r| r.rev).collect();
            revs.sort();
            (c.seq.as_num(), c.id, revs, c.deleted)
        })
        .collect();
    let a_rev1_body = db
        .get_with_opts(
            "a",
            GetOptions {
                rev: Some(fx.a_rev1.clone()),
                ..Default::default()
            },
        )
        .await
        .ok()
        .map(|d| d.to_json());
    let mut indexes: Vec<(Option<String>, String)> = db
        .get_indexes()
        .await
        .into_iter()
        .map(|i| (i.ddoc, i.name))
        .collect();
    indexes.sort();
    Snapshot {
        doc_count: info.doc_count,
        doc_del_count: info.doc_del_count,
        update_seq: info.update_seq.as_num(),
        changes,
        a_rev1_body,
        local: db.adapter().get_local("ck").await.ok(),
        security: serde_json::to_value(db.get_security().await.unwrap()).unwrap(),
        indexes,
        attachment: db.get_attachment("att", "f.bin").await.ok(),
    }
}

/// What a request does when it is allowed through.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Effect {
    /// Reads only: the snapshot must not change even with valid credentials.
    Reads,
    /// Writes: with valid credentials the snapshot must change.
    Writes,
    /// Only the rejection is checked (`DELETE /db` is being reworked).
    RejectionOnly,
}

struct Row {
    method: Method,
    uri: String,
    body: Option<(&'static str, Vec<u8>)>,
    effect: Effect,
    /// Database-level routes use CouchDB's "not authorized to access this
    /// db" reason for anonymous requests; server-level ones differ (CouchDB
    /// says "You are not a server admin."), so only the error name is
    /// checked for them.
    db_level: bool,
}

fn row(method: Method, uri: impl Into<String>, effect: Effect) -> Row {
    let uri = uri.into();
    let db_level = uri.starts_with("/db");
    Row {
        method,
        uri,
        body: None,
        effect,
        db_level,
    }
}

fn json_row(method: Method, uri: impl Into<String>, body: Value, effect: Effect) -> Row {
    Row {
        body: Some(("application/json", body.to_string().into_bytes())),
        ..row(method, uri, effect)
    }
}

/// One row per route × method registered in `routes::build_routes` (plus
/// HEAD on some GET routes), except the public ones.
fn rows(fx: &Fixture) -> Vec<Row> {
    use Effect::*;
    use Method as M;
    let (r2, ddoc_rev, att_rev) = (&fx.a_rev2, &fx.ddoc_rev, &fx.att_rev);
    vec![
        // Server level.
        row(M::GET, "/_all_dbs", Reads),
        row(M::GET, "/_active_tasks", Reads),
        row(M::GET, "/_membership", Reads),
        // Database.
        row(M::GET, "/db", Reads),
        row(M::HEAD, "/db", Reads),
        row(M::PUT, "/db", Reads),
        json_row(M::POST, "/db", json!({"_id": "posted"}), Writes),
        row(M::DELETE, "/db", RejectionOnly),
        // _all_docs, _bulk_docs, _changes.
        row(M::GET, "/db/_all_docs?include_docs=true", Reads),
        json_row(M::POST, "/db/_all_docs", json!({"keys": ["a"]}), Reads),
        json_row(
            M::POST,
            "/db/_bulk_docs",
            json!({"docs": [{"_id": "bulk"}]}),
            Writes,
        ),
        row(M::GET, "/db/_changes", Reads),
        json_row(M::POST, "/db/_changes", json!({}), Reads),
        // Mango.
        json_row(M::POST, "/db/_find", json!({"selector": {}}), Reads),
        json_row(
            M::POST,
            "/db/_explain",
            json!({"selector": {"n": 1}}),
            Reads,
        ),
        row(M::GET, "/db/_index", Reads),
        json_row(
            M::POST,
            "/db/_index",
            json!({"index": {"fields": ["m"]}, "name": "by-m", "ddoc": "idx2"}),
            Writes,
        ),
        json_row(
            M::POST,
            "/db/_index/_bulk_delete",
            json!({"docids": ["_design/idx"]}),
            Writes,
        ),
        row(M::DELETE, "/db/_index/idx/json/by-n", Writes),
        row(M::DELETE, "/db/_index/_design/idx/json/by-n", Writes),
        // Maintenance and replication protocol.
        json_row(M::POST, "/db/_compact", json!({}), Writes),
        json_row(M::POST, "/db/_revs_diff", json!({"a": ["9-x"]}), Reads),
        json_row(
            M::POST,
            "/db/_bulk_get",
            json!({"docs": [{"id": "a"}]}),
            Reads,
        ),
        json_row(M::POST, "/db/_purge", json!({"a": [r2]}), Writes),
        row(M::GET, "/db/_local/ck", Reads),
        json_row(
            M::PUT,
            "/db/_local/ck",
            json!({"_rev": "0-1", "v": 2}),
            Writes,
        ),
        row(M::DELETE, "/db/_local/ck", Writes),
        row(M::GET, "/db/_security", Reads),
        json_row(
            M::PUT,
            "/db/_security",
            json!({"admins": {"names": ["mallory"], "roles": []}}),
            Writes,
        ),
        // Design documents and views.
        row(M::GET, "/db/_design/idx/_view/by-n", Reads),
        json_row(M::POST, "/db/_design/idx/_view/by-n", json!({}), Reads),
        row(M::GET, "/db/_design/d/_info", Reads),
        row(M::GET, "/db/_design/d", Reads),
        json_row(
            M::PUT,
            "/db/_design/d",
            json!({"_rev": ddoc_rev, "views": {}}),
            Writes,
        ),
        row(M::DELETE, format!("/db/_design/d?rev={ddoc_rev}"), Writes),
        // Attachments.
        row(M::GET, "/db/att/f.bin", Reads),
        row(M::HEAD, "/db/att/f.bin", Reads),
        Row {
            body: Some(("text/plain", b"more".to_vec())),
            ..row(M::PUT, format!("/db/att/g.txt?rev={att_rev}"), Writes)
        },
        row(M::DELETE, format!("/db/att/f.bin?rev={att_rev}"), Writes),
        // Documents.
        row(M::GET, "/db/a", Reads),
        row(M::HEAD, "/db/a", Reads),
        json_row(M::PUT, "/db/a", json!({"_rev": r2, "n": 99}), Writes),
        json_row(M::PUT, "/db/fresh", json!({"n": 0}), Writes),
        row(M::DELETE, format!("/db/a?rev={r2}"), Writes),
        // Non-GET methods on `/` are not public (CouchDB answers wrong
        // credentials there with 401 too).
        json_row(M::POST, "/", json!({}), Reads),
        row(M::PUT, "/", Reads),
        row(M::DELETE, "/", Reads),
    ]
}

/// A rejected credential variant and the 401 reason CouchDB gives for it
/// (`None`: anonymous, reason depends on the route).
struct Variant {
    label: &'static str,
    headers: Vec<(&'static str, String)>,
    reason: Option<&'static str>,
}

fn variants(token: &str) -> Vec<Variant> {
    let bad = |label, user: &str, pass: &str| Variant {
        label,
        headers: vec![("authorization", basic(user, pass))],
        reason: Some(BAD_CREDENTIALS),
    };
    vec![
        Variant {
            label: "no credentials",
            headers: vec![],
            reason: None,
        },
        // Same length, one byte off: kills `acc | d` -> `acc & d`.
        bad("same-length password", USER, "s3creX"),
        // Same length, two byte differences that cancel under XOR
        // ('s'^'r' == '3'^'2' == 1): kills `acc | d` -> `acc ^ d`.
        bad("xor-cancelling password", USER, "r2cret"),
        bad("password prefix", USER, "s3c"),
        bad("password extension", USER, "s3cretX"),
        bad("empty password", USER, ""),
        bad("wrong user, same length", "admim", PASS),
        bad("user with other case", "Admin", PASS),
        bad("wrong password", USER, "wrong"),
        // A live session token under another cookie name is not a session.
        Variant {
            label: "token in another cookie",
            headers: vec![("cookie", format!("other={token}"))],
            reason: None,
        },
    ]
}

#[tokio::test]
async fn every_route_rejects_missing_or_wrong_credentials_without_side_effects() {
    let probe = fixture().await;
    let row_count = rows(&probe).len();
    for i in 0..row_count {
        // A fresh database per row, so the positive control can write.
        let fx = fixture().await;
        let row = rows(&fx).swap_remove(i);
        let what = format!("{} {}", row.method, row.uri);
        let before = snapshot(&fx).await;

        for variant in variants(&fx.token) {
            if row.uri == "/" && variant.reason.is_none() {
                // CouchDB answers anonymous non-GET requests on `/` with 405,
                // not 401; only wrong credentials are checked there.
                continue;
            }
            let resp = request(
                &fx.app,
                row.method.clone(),
                &row.uri,
                &variant.headers,
                row.body.clone(),
            )
            .await;
            let ctx = format!("{what} with {}", variant.label);
            assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{ctx}");
            if row.method != Method::HEAD {
                match (variant.reason, row.db_level) {
                    (Some(reason), _) => assert_eq!(resp.json(), unauthorized(reason), "{ctx}"),
                    (None, true) => {
                        assert_eq!(resp.json(), unauthorized(NOT_AUTHORIZED_DB), "{ctx}")
                    }
                    (None, false) => assert_eq!(resp.json()["error"], "unauthorized", "{ctx}"),
                }
            }
            assert_eq!(snapshot(&fx).await, before, "{ctx} changed the database");
        }

        if row.effect == Effect::RejectionOnly {
            continue;
        }
        // Positive control through the session cookie, surrounded by other
        // cookies.
        let cookie = format!("other=x; AuthSession={}; theme=dark", fx.token);
        let resp = request(
            &fx.app,
            row.method.clone(),
            &row.uri,
            &[("cookie", cookie)],
            row.body.clone(),
        )
        .await;
        assert_ne!(
            resp.status,
            StatusCode::UNAUTHORIZED,
            "{what} with a session"
        );
        let after = snapshot(&fx).await;
        match row.effect {
            Effect::Reads => assert_eq!(after, before, "{what} must not write"),
            Effect::Writes => assert_ne!(after, before, "{what} did not write"),
            Effect::RejectionOnly => unreachable!(),
        }
    }
}

#[tokio::test]
async fn wrong_logins_are_rejected_without_a_session() {
    let fx = fixture().await;
    let before = snapshot(&fx).await;
    let attempts = [
        ("admin", "s3creX"),
        ("admin", "r2cret"),
        ("admin", "s3c"),
        ("admin", "s3cretX"),
        ("admin", ""),
        ("admim", "s3cret"),
        ("Admin", "s3cret"),
        ("admin", "wrong"),
    ];
    for (name, password) in attempts {
        let json_body = json!({"name": name, "password": password})
            .to_string()
            .into_bytes();
        let form_body = serde_urlencoded::to_string([("name", name), ("password", password)])
            .unwrap()
            .into_bytes();
        for (ct, body) in [
            ("application/json", json_body),
            ("application/x-www-form-urlencoded", form_body),
        ] {
            let ctx = format!("{name}:{password} as {ct}");
            let resp = request(&fx.app, Method::POST, "/_session", &[], Some((ct, body))).await;
            assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{ctx}");
            assert_eq!(resp.json(), unauthorized(BAD_CREDENTIALS), "{ctx}");
            // The only cookie handed out clears any previous session.
            let cookies: Vec<&str> = resp
                .headers
                .get_all("set-cookie")
                .iter()
                .map(|v| v.to_str().unwrap())
                .collect();
            assert_eq!(
                cookies,
                ["AuthSession=; Version=1; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"],
                "{ctx}"
            );
        }
    }
    // Missing fields are wrong credentials too.
    for body in [
        json!({"name": "admin"}),
        json!({"password": "s3cret"}),
        json!({}),
    ] {
        let resp = request(
            &fx.app,
            Method::POST,
            "/_session",
            &[],
            Some(("application/json", body.to_string().into_bytes())),
        )
        .await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(resp.json(), unauthorized(BAD_CREDENTIALS), "{body}");
    }
    assert_eq!(snapshot(&fx).await, before);
}

#[tokio::test]
async fn public_endpoints_are_reachable_without_credentials() {
    let fx = fixture().await;
    let before = snapshot(&fx).await;
    for (method, uri) in [
        (Method::GET, "/"),
        // `HEAD /` stays public like `GET /`.
        (Method::HEAD, "/"),
        (Method::GET, "/_session"),
        (Method::GET, "/_uuids"),
        (Method::GET, "/_utils"),
        (Method::GET, "/_utils/"),
        (Method::GET, "/_utils/index.html"),
        (Method::GET, "/_utils/some/spa/route"),
    ] {
        let resp = request(&fx.app, method.clone(), uri, &[], None).await;
        assert_eq!(resp.status, StatusCode::OK, "{method} {uri}");
    }
    let resp = request(&fx.app, Method::GET, "/_session", &[], None).await;
    assert_eq!(
        resp.json(),
        json!({
            "ok": true,
            "userCtx": {"name": null, "roles": []},
            "info": {"authentication_handlers": ["cookie", "default"]},
        })
    );
    let resp = request(&fx.app, Method::DELETE, "/_session", &[], None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"ok": true}));
    assert_eq!(snapshot(&fx).await, before);
}

/// Only the `AuthSession` cookie carries the session, wherever it appears in
/// the `Cookie` header (as in CouchDB).
#[tokio::test]
async fn session_cookie_is_found_among_other_cookies() {
    let fx = fixture().await;
    let token = &fx.token;
    let get = |uri: &'static str, cookie: String| {
        let app = fx.app.clone();
        async move { request(&app, Method::GET, uri, &[("cookie", cookie)], None).await }
    };

    for cookie in [
        format!("AuthSession={token}"),
        format!("other=x; AuthSession={token}"),
        format!("other=x;AuthSession={token};last=y"),
    ] {
        assert_eq!(
            get("/db", cookie.clone()).await.status,
            StatusCode::OK,
            "{cookie}"
        );
        assert_eq!(
            get("/_session", cookie.clone()).await.json(),
            json!({
                "ok": true,
                "userCtx": {"name": USER, "roles": ["_admin"]},
                "info": {"authentication_handlers": ["cookie", "default"], "authenticated": "cookie"},
            }),
            "{cookie}"
        );
    }

    for cookie in [
        format!("other={token}"),
        format!("xAuthSession={token}"),
        "other=x; AuthSession=".to_string(),
    ] {
        let resp = get("/db", cookie.clone()).await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{cookie}");
        assert_eq!(resp.json(), unauthorized(NOT_AUTHORIZED_DB), "{cookie}");
        assert_eq!(
            get("/_session", cookie.clone()).await.json(),
            json!({
                "ok": true,
                "userCtx": {"name": null, "roles": []},
                "info": {"authentication_handlers": ["cookie", "default"]},
            }),
            "{cookie}"
        );
    }
}
