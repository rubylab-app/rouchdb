use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;

use crate::state::AppState;

/// Name of the session cookie, matching CouchDB.
pub const SESSION_COOKIE: &str = "AuthSession";

/// Admin credentials required by the server when authentication is enabled.
#[derive(Clone)]
pub struct AdminCredentials {
    pub username: String,
    pub password: String,
}

impl AdminCredentials {
    /// Parse a `user:password` pair (the password may itself contain `:`).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.split_once(':') {
            Some((user, pass)) if !user.is_empty() && !pass.is_empty() => Ok(Self {
                username: user.to_string(),
                password: pass.to_string(),
            }),
            _ => Err("expected credentials in the form user:password".to_string()),
        }
    }

    fn matches(&self, username: &str, password: &str) -> bool {
        // Evaluate both comparisons so the timing does not reveal which one failed.
        let user_ok = constant_time_eq(self.username.as_bytes(), username.as_bytes());
        let pass_ok = constant_time_eq(self.password.as_bytes(), password.as_bytes());
        user_ok & pass_ok
    }
}

// Never print the password, e.g. in `ServerConfig`'s Debug output.
impl std::fmt::Debug for AdminCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Authentication state: the admin credentials plus the live cookie sessions.
pub struct Auth {
    admin: AdminCredentials,
    sessions: Mutex<HashMap<String, Instant>>,
    /// Sessions that are not used for this long are forgotten.
    timeout: Duration,
}

/// Result of inspecting the credentials sent with a request.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    /// Valid Basic credentials or session cookie.
    Admin,
    /// Basic credentials were sent but are wrong.
    BadCredentials,
    /// No usable credentials.
    Anonymous,
}

impl Auth {
    /// Authentication with CouchDB's default session timeout (10 minutes).
    pub fn new(admin: AdminCredentials) -> Self {
        Self::with_timeout(admin, crate::DEFAULT_SESSION_TIMEOUT)
    }

    /// Authentication whose cookie sessions expire after `timeout` without
    /// use.
    pub fn with_timeout(admin: AdminCredentials, timeout: Duration) -> Self {
        Self {
            admin,
            sessions: Mutex::new(HashMap::new()),
            timeout,
        }
    }

    /// The `Set-Cookie` value for a session token. `Max-Age` is the session
    /// timeout, rounded up to whole seconds (0 would delete the cookie).
    pub fn session_cookie(&self, token: &str) -> String {
        let max_age = self.timeout.as_secs() + u64::from(self.timeout.subsec_nanos() > 0);
        format!(
            "{SESSION_COOKIE}={token}; Version=1; Max-Age={max_age}; Path=/; HttpOnly; SameSite=Strict"
        )
    }

    pub fn username(&self) -> &str {
        &self.admin.username
    }

    pub fn check_password(&self, username: &str, password: &str) -> bool {
        self.admin.matches(username, password)
    }

    /// Inspect the `Authorization` header and the session cookie.
    pub fn authenticate(&self, headers: &HeaderMap) -> AuthOutcome {
        if let Some((user, pass)) = basic_credentials(headers) {
            return if self.check_password(&user, &pass) {
                AuthOutcome::Admin
            } else {
                AuthOutcome::BadCredentials
            };
        }
        match session_token(headers) {
            Some(token) if self.touch_session(&token) => AuthOutcome::Admin,
            _ => AuthOutcome::Anonymous,
        }
    }

    /// Start a new cookie session and return its token.
    pub fn create_session(&self) -> String {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, last_seen| now.duration_since(*last_seen) < self.timeout);
        sessions.insert(token.clone(), now);
        token
    }

    /// Forget the session carried by the request, if any.
    pub fn end_session(&self, headers: &HeaderMap) {
        if let Some(token) = session_token(headers) {
            self.sessions.lock().unwrap().remove(&token);
        }
    }

    fn touch_session(&self, token: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        match sessions.get_mut(token) {
            Some(last_seen) if now.duration_since(*last_seen) < self.timeout => {
                *last_seen = now;
                true
            }
            Some(_) => {
                sessions.remove(token);
                false
            }
            None => false,
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Decode `Authorization: Basic base64(user:pass)`.
fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// Extract the `AuthSession` cookie value.
fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, value)| *name == SESSION_COOKIE && !value.is_empty())
        .map(|(_, value)| value.to_string())
}

/// Endpoints that stay reachable without credentials, as in CouchDB: the
/// welcome message, session login/logout, UUIDs and the static Fauxton files.
fn is_public(method: &Method, path: &str) -> bool {
    match path {
        "/" => method == Method::GET || method == Method::HEAD,
        "/_session" | "/_uuids" | "/_utils" => true,
        _ => path.starts_with("/_utils/"),
    }
}

/// Middleware: when admin credentials are configured, reject wrong Basic
/// credentials on every endpoint (as CouchDB does, even on the public ones)
/// and every non-public request that does not carry valid credentials
/// (Basic auth or session cookie).
///
/// A request authenticated by its session cookie gets the cookie back with a
/// fresh `Max-Age`, so an active session is not dropped by the browser.
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(auth) = state.auth.as_deref() else {
        return next.run(req).await;
    };
    let outcome = auth.authenticate(req.headers());
    let path = req.uri().path();
    match outcome {
        AuthOutcome::BadCredentials => {
            return unauthorized("Name or password is incorrect.");
        }
        AuthOutcome::Anonymous if is_public(req.method(), path) => return next.run(req).await,
        // Server-level endpoints need a server admin; the others a database member.
        AuthOutcome::Anonymous if path.starts_with("/_") => {
            return unauthorized("You are not a server admin.");
        }
        AuthOutcome::Anonymous => {
            return unauthorized("You are not authorized to access this db.");
        }
        AuthOutcome::Admin => {}
    }

    // Basic credentials take precedence over the cookie in `authenticate`.
    let session = if basic_credentials(req.headers()).is_none() {
        session_token(req.headers())
    } else {
        None
    };
    let mut resp = next.run(req).await;
    if let Some(token) = session
        && !resp.headers().contains_key(header::SET_COOKIE)
        && let Ok(cookie) = auth.session_cookie(&token).parse()
    {
        resp.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    resp
}

pub fn unauthorized(reason: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": "unauthorized",
            "reason": reason,
        })),
    )
        .into_response()
}
