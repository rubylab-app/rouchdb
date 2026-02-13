use axum::http::StatusCode;
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
