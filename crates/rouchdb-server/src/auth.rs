use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
// tokio's Instant behaves like std's at runtime, but tests can pause and
// advance it to exercise session expiry.
use tokio::time::Instant;

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
    /// Whether `X-Forwarded-Proto` comes from a trusted reverse proxy.
    trust_proxy: bool,
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
            trust_proxy: false,
        }
    }

    /// Trust the `X-Forwarded-Proto` header of the requests (set by a
    /// reverse proxy that terminates TLS): when it says `https`, session
    /// cookies get the `Secure` attribute. Off by default, since a client
    /// talking to the server directly could send any value.
    pub fn trust_proxy(mut self, trust: bool) -> Self {
        self.trust_proxy = trust;
        self
    }

    /// The `Set-Cookie` value for a session token issued in answer to a
    /// request with these headers. `Max-Age` is the session timeout, rounded
    /// up to whole seconds (0 would delete the cookie). The cookie is
    /// `Secure` only when the proxy is trusted and says the client connected
    /// over HTTPS: a `Secure` cookie would not come back over plain HTTP, so
    /// logging in on `http://localhost` would not work.
    pub fn session_cookie(&self, token: &str, headers: &HeaderMap) -> String {
        let max_age = self.timeout.as_secs() + u64::from(self.timeout.subsec_nanos() > 0);
        let secure = if self.trust_proxy && forwarded_https(headers) {
            "; Secure"
        } else {
            ""
        };
        format!(
            "{SESSION_COOKIE}={token}; Version=1; Max-Age={max_age}; Path=/; HttpOnly; SameSite=Strict{secure}"
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
    ///
    /// Tokens have the shape of CouchDB's (unpadded base64url of
    /// `name:hex time:hash`, here a random hash), so the malformed-cookie
    /// check treats both alike.
    pub fn create_session(&self) -> String {
        let unix_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
            "{}:{:X}:{}",
            self.admin.username,
            unix_time,
            uuid::Uuid::new_v4().simple()
        ));
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

/// Whether a reverse proxy says the client connected over HTTPS: the first
/// (client-side) entry of `X-Forwarded-Proto` is `https`.
fn forwarded_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
}

/// Compare two secrets in a time that does not depend on their contents, and
/// without stopping early when their lengths differ: every byte up to the
/// longer length is compared (the shorter one padded with zeros) and the
/// length difference is folded into the result.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    // Keep the optimizer from turning the loop into an early exit.
    std::hint::black_box(diff) == 0
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

/// The value of the first `AuthSession` cookie, if there is one: like
/// CouchDB, a later one is ignored, even when the first is empty.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
}

/// The session token the request carries: its first `AuthSession` cookie,
/// unless empty.
fn session_token(headers: &HeaderMap) -> Option<String> {
    session_cookie(headers)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

/// Whether a session cookie value cannot be a CouchDB one: unpadded
/// base64url (CouchDB adds the padding itself) of at least three
/// `:`-separated parts (`name:time:hash`). CouchDB answers such a cookie
/// with a 400 on every request.
fn is_malformed_session(value: &str) -> bool {
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    const BASE64URL: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::RequireNone)
            .with_decode_allow_trailing_bits(true),
    );
    BASE64URL.decode(value).map_or(true, |decoded| {
        decoded.iter().filter(|&&b| b == b':').count() < 2
    })
}

/// Requests that need no credentials, as in CouchDB: the welcome message,
/// session login/logout, UUIDs and the static Fauxton files, and the
/// methods `/` and `/_active_tasks` do not allow (CouchDB checks the method
/// of these before the credentials, so they are a 405, not a 401).
fn is_public(method: &Method, path: &str) -> bool {
    let reads = method == Method::GET || method == Method::HEAD;
    match path {
        "/" => true,
        "/_active_tasks" => !reads,
        "/_session" | "/_uuids" => true,
        "/_utils" => reads,
        _ => reads && path.starts_with("/_utils/"),
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
    if session_cookie(req.headers()).is_some_and(|v| !v.is_empty() && is_malformed_session(v)) {
        return crate::error::couch_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Malformed AuthSession cookie. Please clear your cookies.",
        );
    }
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
    let cookie = if basic_credentials(req.headers()).is_none() {
        session_token(req.headers()).map(|token| auth.session_cookie(&token, req.headers()))
    } else {
        None
    };
    let mut resp = next.run(req).await;
    if let Some(cookie) = cookie
        && !resp.headers().contains_key(header::SET_COOKIE)
        && let Ok(cookie) = cookie.parse()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_compares_contents_and_lengths() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"s3cret", b"s3cret"));
        for (a, b) in [
            (&b"s3cret"[..], &b"s3creT"[..]),
            (b"s3cret", b"S3cret"),
            (b"s3cret", b"s3cre"),
            (b"s3cret", b"s3cret!"),
            (b"", b"x"),
            // Zero padding must not make a prefix equal: the length
            // difference counts, even when it is a multiple of 256.
            (b"abc", b"abc\0"),
            (b"", &[0u8; 256]),
        ] {
            assert!(!constant_time_eq(a, b), "{a:?} == {b:?}");
            assert!(!constant_time_eq(b, a), "{b:?} == {a:?}");
        }
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn session_cookie_is_secure_only_behind_a_trusted_https_proxy() {
        let admin = AdminCredentials::parse("admin:s3cret").unwrap();
        let plain = "AuthSession=t; Version=1; Max-Age=600; Path=/; HttpOnly; SameSite=Strict";
        let secure = format!("{plain}; Secure");
        let https = headers(&[("x-forwarded-proto", "https")]);

        let untrusted = Auth::new(admin.clone());
        assert_eq!(untrusted.session_cookie("t", &https), plain);
        assert_eq!(untrusted.session_cookie("t", &HeaderMap::new()), plain);

        let trusted = Auth::new(admin).trust_proxy(true);
        assert_eq!(trusted.session_cookie("t", &HeaderMap::new()), plain);
        for (value, is_secure) in [
            ("https", true),
            ("HTTPS", true),
            (" https ", true),
            ("https, http", true),
            ("http", false),
            ("http, https", false),
            ("", false),
            ("httpsx", false),
        ] {
            let map = headers(&[("x-forwarded-proto", value)]);
            let expected = if is_secure { secure.as_str() } else { plain };
            assert_eq!(trusted.session_cookie("t", &map), expected, "{value:?}");
        }
    }
}
