//! JSON request bodies holding documents as deeply nested as the database
//! stores them.
//!
//! axum's `Json` decodes with serde_json, which stops at 128 levels; the
//! database stores documents up to `MAX_NESTING_DEPTH` levels deep, so
//! every body is decoded with [`rouchdb_core::json::from_input`] instead.

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rouchdb_core::error::RouchError;
use serde::de::DeserializeOwned;

use crate::error::{AppError, JSON_CONTENT_TYPE_REQUIRED, couch_error};

/// Levels a request body may add around the documents it carries, such as
/// `{"docs": [...]}` in `_bulk_docs`.
const BODY_ENVELOPE_DEPTH: usize = 2;

/// Decode a request body. A body nested deeper than the documents allow is
/// the 400 a too-deep document write gets; malformed JSON is CouchDB's
/// `invalid UTF-8 JSON`, and JSON of the wrong shape a plain 400.
pub(crate) fn decode<T>(body: &[u8]) -> Result<T, AppError>
where
    T: DeserializeOwned + Send + 'static,
{
    rouchdb_core::json::from_input(body, BODY_ENVELOPE_DEPTH).map_err(|e| {
        AppError(match e {
            RouchError::Json(e) if e.is_data() => RouchError::BadRequest(e.to_string()),
            RouchError::Json(_) => RouchError::BadRequest("invalid UTF-8 JSON".into()),
            other => other,
        })
    })
}

/// `application/json`, or an `application/*+json` type, as axum's `Json`
/// requires.
fn is_json(headers: &HeaderMap) -> bool {
    let Some(content_type) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match essence.split_once('/') {
        Some(("application", subtype)) => subtype == "json" || subtype.ends_with("+json"),
        _ => false,
    }
}

/// A JSON request body: like axum's `Json` extractor (the request must be
/// `application/json`, else 415), decoded with [`decode`].
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !is_json(req.headers()) {
            return Err(couch_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "bad_content_type",
                JSON_CONTENT_TYPE_REQUIRED,
            ));
        }
        let body = Bytes::from_request(req, state)
            .await
            .map_err(IntoResponse::into_response)?;
        decode(&body)
            .map(JsonBody)
            .map_err(IntoResponse::into_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_content_types() {
        let accepts = |ct: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, ct.parse().unwrap());
            is_json(&headers)
        };
        for ct in [
            "application/json",
            "application/json; charset=utf-8",
            "Application/JSON",
            "application/vnd.api+json",
        ] {
            assert!(accepts(ct), "{ct}");
        }
        for ct in [
            "text/plain",
            "application/jsonx",
            "text/json",
            "application",
            "multipart/form-data",
        ] {
            assert!(!accepts(ct), "{ct}");
        }
        assert!(!is_json(&HeaderMap::new()));
    }
}
