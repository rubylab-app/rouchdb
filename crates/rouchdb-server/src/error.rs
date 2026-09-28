use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rouchdb_core::error::RouchError;

/// Wrapper around `RouchError` that implements `IntoResponse` for Axum.
pub struct AppError(pub RouchError);

impl From<RouchError> for AppError {
    fn from(err: RouchError) -> Self {
        AppError(err)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error, reason) = match &self.0 {
            RouchError::NotFound(msg) => (StatusCode::NOT_FOUND, "not_found", msg.clone()),
            RouchError::Conflict => (
                StatusCode::CONFLICT,
                "conflict",
                "Document update conflict".to_string(),
            ),
            RouchError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg.clone()),
            RouchError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "You are not authorized".to_string(),
            ),
            RouchError::Forbidden(msg) => (StatusCode::FORBIDDEN, "forbidden", msg.clone()),
            RouchError::DatabaseExists(msg) => {
                (StatusCode::PRECONDITION_FAILED, "file_exists", msg.clone())
            }
            RouchError::InvalidRev(msg) => (
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("Invalid rev: {msg}"),
            ),
            RouchError::MissingId => (
                StatusCode::BAD_REQUEST,
                "bad_request",
                "Missing document id".to_string(),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_server_error",
                self.0.to_string(),
            ),
        };

        let body = serde_json::json!({
            "error": error,
            "reason": reason,
        });

        (status, axum::Json(body)).into_response()
    }
}

/// CouchDB error name for an HTTP status.
fn error_name(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::NOT_ACCEPTABLE => "not_acceptable",
        StatusCode::CONFLICT => "conflict",
        StatusCode::PRECONDITION_FAILED => "precondition_failed",
        StatusCode::PAYLOAD_TOO_LARGE => "too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "bad_content_type",
        _ if status.is_client_error() => "bad_request",
        _ => "unknown_error",
    }
}

/// Response middleware: turn the plain-text (or empty) error responses that
/// axum produces itself — extractor rejections, body-limit errors, unmatched
/// routes and methods — into CouchDB-style `{"error", "reason"}` JSON.
pub async fn json_errors(response: Response) -> Response {
    let status = response.status();
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));
    if !(status.is_client_error() || status.is_server_error()) || is_json {
        return response;
    }

    // A well-formed JSON body of the wrong shape is a plain bad request in
    // CouchDB, not axum's 422.
    let status = if status == StatusCode::UNPROCESSABLE_ENTITY {
        StatusCode::BAD_REQUEST
    } else {
        status
    };
    let allow = response
        .headers()
        .get(header::ALLOW)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let text = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .unwrap_or_default();

    let reason = match status {
        StatusCode::PAYLOAD_TOO_LARGE => "the request entity is too large".to_string(),
        StatusCode::METHOD_NOT_ALLOWED => match &allow {
            Some(allow) => format!("Only {allow} allowed"),
            None => "method not allowed".to_string(),
        },
        _ if text.is_empty() => status.canonical_reason().unwrap_or("error").to_string(),
        _ => text,
    };

    let mut resp = (
        status,
        axum::Json(serde_json::json!({
            "error": error_name(status),
            "reason": reason,
        })),
    )
        .into_response();
    if let Some(allow) = allow.and_then(|a| a.parse().ok()) {
        resp.headers_mut().insert(header::ALLOW, allow);
    }
    resp
}
