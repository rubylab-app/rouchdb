use std::collections::HashMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::auth::{AuthOutcome, unauthorized};
use crate::state::AppState;

const CLEAR_COOKIE: &str = "AuthSession=; Version=1; Path=/; HttpOnly; SameSite=Strict; Max-Age=0";

/// GET /_session — current session.
///
/// Without configured credentials every client is the admin (the historical
/// "admin party" stub). Otherwise report who the request authenticates as.
pub async fn get_session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(auth) = state.auth.as_deref() else {
        return session_with_cookie(StatusCode::OK);
    };

    let handler = if headers.contains_key(header::AUTHORIZATION) {
        "default"
    } else {
        "cookie"
    };
    match auth.authenticate(&headers) {
        AuthOutcome::Admin => Json(serde_json::json!({
            "ok": true,
            "userCtx": { "name": auth.username(), "roles": ["_admin"] },
            "info": {
                "authentication_handlers": ["cookie", "default"],
                "authenticated": handler,
            },
        }))
        .into_response(),
        AuthOutcome::BadCredentials => unauthorized("Name or password is incorrect."),
        AuthOutcome::Anonymous => Json(serde_json::json!({
            "ok": true,
            "userCtx": { "name": null, "roles": [] },
            "info": { "authentication_handlers": ["cookie", "default"] },
        }))
        .into_response(),
    }
}

/// POST /_session — log in with JSON or form-encoded `name` / `password`.
///
/// Without configured credentials any login succeeds (stub); otherwise the
/// credentials are checked and a session cookie is issued.
pub async fn post_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(auth) = state.auth.as_deref() else {
        return session_with_cookie(StatusCode::OK);
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let fields: HashMap<String, serde_json::Value> = if content_type.starts_with("application/json")
    {
        serde_json::from_slice(&body).unwrap_or_default()
    } else if content_type.starts_with("application/x-www-form-urlencoded") {
        serde_urlencoded::from_bytes::<HashMap<String, String>>(&body)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect()
    } else {
        return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                Json(serde_json::json!({
                    "error": "bad_content_type",
                    "reason": "Content-Type must be 'application/x-www-form-urlencoded' or 'application/json'",
                })),
            )
                .into_response();
    };

    let field = |k: &str| fields.get(k).and_then(|v| v.as_str());
    match (field("name"), field("password")) {
        (Some(name), Some(password)) if auth.check_password(name, password) => {
            let token = auth.create_session();
            let cookie = auth.session_cookie(&token, &headers);
            (
                StatusCode::OK,
                [(header::SET_COOKIE, cookie)],
                Json(serde_json::json!({
                    "ok": true,
                    "name": name,
                    "roles": ["_admin"],
                    "userCtx": { "name": name, "roles": ["_admin"] },
                })),
            )
                .into_response()
        }
        _ => {
            let mut resp = unauthorized("Name or password is incorrect.");
            resp.headers_mut()
                .insert(header::SET_COOKIE, HeaderValue::from_static(CLEAR_COOKIE));
            resp
        }
    }
}

/// DELETE /_session — log out (forgets the session and clears the cookie).
pub async fn delete_session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(auth) = state.auth.as_deref() {
        auth.end_session(&headers);
    }
    let body = serde_json::json!({"ok": true});
    (
        StatusCode::OK,
        [(header::SET_COOKIE, CLEAR_COOKIE)],
        Json(body),
    )
        .into_response()
}

fn session_with_cookie(status: StatusCode) -> Response {
    let body = session_response();
    (
        status,
        [(header::SET_COOKIE, "AuthSession=YWRtaW46NjdBQkE3ODE6stHxxBdC_ZKOnMSPCkDNxVFsgeQ; Version=1; Path=/; HttpOnly")],
        Json(body),
    )
        .into_response()
}

fn session_response() -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "userCtx": {
            "name": "admin",
            "roles": ["_admin"],
        },
        "info": {
            "authentication_handlers": ["cookie", "default"],
            "authenticated": "cookie",
        },
    })
}
