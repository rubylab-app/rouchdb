//! Raw JSON requests against database-level endpoints that the `Adapter`
//! trait does not cover (such as Mango's `_find` and `_index`).

use rouchdb_core::error::{Result, RouchError};

use crate::HttpAdapter;

impl HttpAdapter {
    /// Send a JSON request to a path relative to the database URL (for
    /// example `_find` or `_index`) and return the HTTP status and the JSON
    /// body (`null` if the body is empty or not JSON).
    ///
    /// Error statuses are returned as they are, not turned into errors;
    /// only transport failures and invalid methods are.
    pub async fn request_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(u16, serde_json::Value)> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| RouchError::BadRequest(format!("invalid HTTP method: {e}")))?;
        let mut request = self.client.request(method, self.url(path));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        Ok((status, json))
    }
}
