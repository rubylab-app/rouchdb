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
        let (status, error, reason) = match self.0 {
            RouchError::NotFound(msg) => (StatusCode::NOT_FOUND, "not_found", msg),
            RouchError::Conflict => (
                StatusCode::CONFLICT,
                "conflict",
                "Document update conflict.".to_string(),
            ),
            RouchError::BadRequest(msg) => (StatusCode::BAD_REQUEST, bad_request_name(&msg), msg),
            RouchError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "You are not authorized".to_string(),
            ),
            RouchError::Forbidden(msg) => (StatusCode::FORBIDDEN, "forbidden", msg),
            RouchError::DatabaseExists(msg) => {
                (StatusCode::PRECONDITION_FAILED, "file_exists", msg)
            }
            RouchError::InvalidRev(_) => (
                StatusCode::BAD_REQUEST,
                "bad_request",
                "Invalid rev format".to_string(),
            ),
            RouchError::MissingId => (
                StatusCode::BAD_REQUEST,
                "bad_request",
                "Missing document id".to_string(),
            ),
            other => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_server_error",
                other.to_string(),
            ),
        };
        couch_error(status, error, &reason)
    }
}

/// A CouchDB-style `{"error": ..., "reason": ...}` response.
pub fn couch_error(status: StatusCode, error: &str, reason: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": error,
            "reason": reason,
        })),
    )
        .into_response()
}

/// CouchDB's reason for a request that must be `application/json`.
pub const JSON_CONTENT_TYPE_REQUIRED: &str = "Content-Type must be application/json";

/// The CouchDB error name of a 400 response.
///
/// `RouchError::BadRequest` only carries the reason. The reasons below are
/// CouchDB's own texts (produced verbatim by the document validation, the
/// Mango engine and the query-string parsing), and each maps to the error name
/// CouchDB sends with it; any other reason is a plain `bad_request`.
pub fn bad_request_name(reason: &str) -> &'static str {
    const PREFIXES: [(&str, &str); 13] = [
        (
            "Only reserved document ids may start with underscore.",
            "illegal_docid",
        ),
        ("Document id must not be empty", "illegal_docid"),
        ("Document id must be a string", "illegal_docid"),
        ("Bad special document member: ", "doc_validation"),
        ("Invalid operator: ", "invalid_operator"),
        ("Bad argument for operator ", "bad_arg"),
        (
            "One or more conditions is missing a field name.",
            "invalid_selector",
        ),
        ("Invalid field name: ", "invalid_field_name"),
        ("Invalid sort field: ", "invalid_sort_field"),
        (
            "Selector must be a JSON object, not: ",
            "invalid_selector_json",
        ),
        ("Missing required key: ", "missing_required_key"),
        ("Invalid value for ", "query_parse_error"),
        ("Invalid boolean parameter: ", "query_parse_error"),
    ];
    PREFIXES
        .iter()
        .find(|(prefix, _)| reason.starts_with(prefix))
        .map_or("bad_request", |(_, name)| name)
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

    // CouchDB lists the allowed methods sorted: `DELETE,GET,HEAD,POST`.
    let allow = allow.map(|allow| {
        let mut methods: Vec<&str> = allow.split(',').map(str::trim).collect();
        methods.sort_unstable();
        methods.dedup();
        methods.join(",")
    });

    let reason = match status {
        StatusCode::PAYLOAD_TOO_LARGE => "the request entity is too large".to_string(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => JSON_CONTENT_TYPE_REQUIRED.to_string(),
        StatusCode::METHOD_NOT_ALLOWED => match &allow {
            Some(allow) => format!("Only {allow} allowed"),
            None => "method not allowed".to_string(),
        },
        _ if text.is_empty() => status.canonical_reason().unwrap_or("error").to_string(),
        _ => text,
    };

    let mut resp = couch_error(status, error_name(status), &reason);
    if let Some(allow) = allow.and_then(|a| a.parse().ok()) {
        resp.headers_mut().insert(header::ALLOW, allow);
    }
    resp
}
