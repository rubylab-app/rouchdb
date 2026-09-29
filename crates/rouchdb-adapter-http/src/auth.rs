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

/// `POST /_session` reply: CouchDB puts `name` and `roles` at the top level
/// here (only `GET /_session` nests them in `userCtx`); accept both.
#[derive(Deserialize)]
struct LoginResponse {
    ok: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(rename = "userCtx", default)]
    user_ctx: Option<UserContext>,
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
        let client =
            crate::client_builder(crate::DEFAULT_CONNECT_TIMEOUT, crate::DEFAULT_READ_TIMEOUT)
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
        let resp = crate::check_response(resp).await?;

        let login = resp
            .json::<LoginResponse>()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        Ok(Session {
            ok: login.ok,
            user_ctx: login.user_ctx.unwrap_or(UserContext {
                name: login.name,
                roles: login.roles,
            }),
        })
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
        let user_id = format!("org.couchdb.user:{}", username);
        let user_doc = serde_json::json!({
            "_id": user_id,
            "name": username,
            "password": password,
            "roles": roles,
            "type": "user"
        });

        let resp = self
            .client
            .put(user_doc_url(&self.server_url, &user_id))
            .json(&user_doc)
            .send()
            .await
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        crate::check_response(resp).await?;

        Ok(())
    }
}

/// URL of a `_users` document. The id is percent-encoded so a name with `/`,
/// `#`, `?` or spaces cannot address another resource.
fn user_doc_url(server_url: &str, user_id: &str) -> String {
    format!("{}/_users/{}", server_url, crate::urlencoded(user_id))
}

#[cfg(test)]
mod tests {
    use super::AuthClient;
    use crate::tests::{json_response, recording_stub_server};

    #[tokio::test]
    async fn login_reads_couchdb_session_reply() {
        // Verbatim CouchDB 3 reply to POST /_session: no `userCtx`.
        let (url, _) = recording_stub_server(json_response(
            "200 OK",
            r#"{"ok":true,"name":"admin","roles":["_admin"]}"#,
        ))
        .await;
        let session = AuthClient::new(&url)
            .login("admin", "password")
            .await
            .unwrap();
        assert!(session.ok);
        assert_eq!(session.user_ctx.name.as_deref(), Some("admin"));
        assert_eq!(session.user_ctx.roles, vec!["_admin"]);
    }

    #[tokio::test]
    async fn logout_deletes_the_server_session() {
        let (url, requests) =
            recording_stub_server(json_response("200 OK", r#"{"ok":true}"#)).await;
        let auth = AuthClient::new(&format!("{url}/"));
        assert_eq!(auth.server_url(), url);

        auth.logout().await.unwrap();
        assert_eq!(*requests.lock().unwrap(), vec!["DELETE /_session HTTP/1.1"]);
    }

    #[tokio::test]
    async fn sign_up_escapes_the_user_id() {
        let (url, requests) = recording_stub_server(json_response(
            "201 Created",
            r#"{"ok":true,"id":"org.couchdb.user:x","rev":"1-a"}"#,
        ))
        .await;
        let auth = AuthClient::new(&url);
        auth.sign_up("bob/evil#x?y z", "pw", vec![]).await.unwrap();

        // One request, for the whole id, not an attachment of
        // `org.couchdb.user:bob` with the rest cut off as a fragment.
        assert_eq!(
            *requests.lock().unwrap(),
            vec!["PUT /_users/org.couchdb.user%3Abob%2Fevil%23x%3Fy%20z HTTP/1.1"]
        );
    }
}
