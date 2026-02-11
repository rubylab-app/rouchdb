/// CouchDB authentication helpers.
///
/// Supports cookie-based authentication (`_session` endpoint),
/// session inspection, and user signup.
use reqwest::Client;
use serde::{Deserialize, Serialize};

use rouchdb_core::error::{Result, RouchError};

/// A CouchDB session response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub ok: bool,
    #[serde(rename = "userCtx")]
    pub user_ctx: UserContext,
}

/// User context from a session response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserContext {
    pub name: Option<String>,
    pub roles: Vec<String>,
}

/// A client that handles CouchDB authentication.
///
/// Uses cookie-based auth (`_session` endpoint). The internal `reqwest::Client`
/// has a cookie store enabled, so after `login()` all subsequent requests
/// automatically include the auth cookie.
pub struct AuthClient {
    client: Client,
    server_url: String,
}

impl AuthClient {
    /// Create a new auth client for the given CouchDB server URL.
    pub fn new(server_url: &str) -> Self {
        let client = Client::builder()
            .cookie_store(true)
            .build()
            .unwrap_or_default();
        Self {
            client,
            server_url: server_url.trim_end_matches('/').to_string(),
        }
    }

    /// Get the underlying reqwest client (with cookie store).
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Get the server URL.
    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    /// Log in with username and password (cookie-based auth).
    pub async fn login(&self, username: &str, password: &str) -> Result<Session> {
        let resp = self
            .client
            .post(format!("{}/_session", self.server_url))
            .json(&serde_json::json!({"name": username, "password": password}))
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(RouchError::DatabaseError(format!(
                "login failed ({}): {}",
                status, body
            )));
        }

        resp.json::<Session>()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))
    }

    /// Log out (delete session cookie).
    pub async fn logout(&self) -> Result<()> {
        self.client
            .delete(format!("{}/_session", self.server_url))
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    /// Get the current session.
    pub async fn get_session(&self) -> Result<Session> {
        let resp = self
            .client
            .get(format!("{}/_session", self.server_url))
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        resp.json::<Session>()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))
    }

    /// Create a new user in the `_users` database.
    pub async fn sign_up(&self, username: &str, password: &str, roles: Vec<String>) -> Result<()> {
        let user_doc = serde_json::json!({
            "_id": format!("org.couchdb.user:{}", username),
            "name": username,
            "password": password,
            "roles": roles,
            "type": "user"
        });

        let resp = self
            .client
            .put(format!(
                "{}/_users/org.couchdb.user:{}",
                self.server_url, username
            ))
            .json(&user_doc)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(RouchError::DatabaseError(format!(
                "signup failed ({}): {}",
                status, body
            )));
        }

        Ok(())
    }
}
