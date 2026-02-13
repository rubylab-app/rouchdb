use axum::Json;
use axum::body::Bytes;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

/// GET /_session — stub admin session (no auth).
pub async fn get_session() -> Response {
    session_with_cookie(StatusCode::OK)
}

/// POST /_session — stub login (accepts any credentials: JSON or form-encoded).
pub async fn post_session(_body: Bytes) -> Response {
    session_with_cookie(StatusCode::OK)
}

/// DELETE /_session — stub logout (clears cookie).
pub async fn delete_session() -> Response {
    let body = serde_json::json!({"ok": true});
    (
        StatusCode::OK,
        [(header::SET_COOKIE, "AuthSession=; Version=1; Path=/; HttpOnly; Max-Age=0")],
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
