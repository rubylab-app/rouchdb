//! `Host` header validation, against DNS rebinding.
//!
//! A web page on `http://attacker.example` can have its DNS name point at
//! `127.0.0.1` once the page is loaded: the browser then sends the page's
//! requests to the local server as *same-origin* requests, so CORS does not
//! stop them and the page reads the answers. The browser still names the
//! attacker's host in the `Host` header, so a server that only answers the
//! names it knows is not reachable this way.
//!
//! On a loopback address the check is always on: the server answers
//! `localhost`, `127.0.0.1`, `[::1]` and the address it listens on (with any
//! port), plus [`ServerConfig::allowed_hosts`]. On any other address it is on
//! only when `allowed_hosts` is not empty. A reverse proxy that forwards
//! another `Host` (its public name) to a loopback server must be declared in
//! `allowed_hosts` (`--allowed-host`).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::ServerConfig;

/// A host name or IP address, as compared by the check: names in lower
/// case, addresses by value (so `[0:0:0:0:0:0:0:1]` is `[::1]`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostName {
    Ip(IpAddr),
    Name(String),
}

impl HostName {
    /// A host without port: a name, an IPv4 address, or an IPv6 address with
    /// or without brackets.
    fn parse(host: &str) -> Option<Self> {
        if let Some(inner) = host.strip_prefix('[') {
            let ip = inner.strip_suffix(']')?.parse::<Ipv6Addr>().ok()?;
            return Some(Self::Ip(IpAddr::V6(ip)));
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Some(Self::Ip(ip));
        }
        let valid = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b));
        valid.then(|| Self::Name(host.to_ascii_lowercase()))
    }

    /// The host of a `Host` header value (`host[:port]`, IPv6 in brackets),
    /// or `None` when the value is malformed.
    fn from_header(value: &str) -> Option<Self> {
        let (host, port) = if value.starts_with('[') {
            let end = value.find(']')?;
            let (host, rest) = value.split_at(end + 1);
            match rest {
                "" => (host, None),
                _ => (host, Some(rest.strip_prefix(':')?)),
            }
        } else {
            match value.split_once(':') {
                // An IPv6 address must be in brackets.
                Some((_, port)) if port.contains(':') => return None,
                Some((host, port)) => (host, Some(port)),
                None => (value, None),
            }
        };
        // RFC 3986 `port = *DIGIT` (possibly empty).
        if port.is_some_and(|p| !p.bytes().all(|b| b.is_ascii_digit())) {
            return None;
        }
        Self::parse(host)
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]"),
            Self::Ip(ip) => write!(f, "{ip}"),
            Self::Name(name) => f.write_str(name),
        }
    }
}

/// Validate a `--allowed-host` value (a host name or an IP address, without
/// scheme or port) and normalize it: lower case, IPv6 in brackets.
pub fn parse_allowed_host(value: &str) -> Result<String, String> {
    let host = value.trim();
    HostName::parse(host).map(|h| h.to_string()).ok_or_else(|| {
        format!(
            "invalid allowed host {value:?}: expected a host name or an IP address, \
             without scheme or port (e.g. db.example.com or 192.168.1.10)"
        )
    })
}

/// Whether `host` (a `--host` value) is a loopback address or `localhost`.
pub(crate) fn is_loopback(host: &str) -> bool {
    match HostName::parse(host.trim()) {
        Some(HostName::Ip(ip)) => ip.is_loopback(),
        Some(HostName::Name(name)) => name == "localhost",
        None => false,
    }
}

/// The host names the server answers to.
#[derive(Debug, Clone)]
pub struct HostAllowlist {
    allowed: Vec<HostName>,
}

impl HostAllowlist {
    /// The allowlist for `config`, or `None` when the `Host` header is not
    /// checked (a non-loopback address without `allowed_hosts`).
    ///
    /// Entries of `allowed_hosts` that [`parse_allowed_host`] rejects are
    /// ignored here; [`ServerConfig::validate`] reports them.
    pub fn from_config(config: &ServerConfig) -> Option<Self> {
        if !is_loopback(&config.host) && config.allowed_hosts.is_empty() {
            return None;
        }
        let mut allowed = vec![
            HostName::Name("localhost".into()),
            HostName::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            HostName::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ];
        let extra = std::iter::once(&config.host).chain(&config.allowed_hosts);
        for host in extra.filter_map(|h| HostName::parse(h.trim())) {
            if !allowed.contains(&host) {
                allowed.push(host);
            }
        }
        Some(Self { allowed })
    }

    /// Whether a `Host` header value (`host[:port]`) names this server; the
    /// port is not checked.
    pub fn allows(&self, value: &str) -> bool {
        HostName::from_header(value).is_some_and(|host| self.allowed.contains(&host))
    }

    /// The accepted hosts, for the startup message.
    pub fn hosts(&self) -> Vec<String> {
        self.allowed.iter().map(ToString::to_string).collect()
    }
}

/// Middleware: answer 400 to a request whose `Host` header (or, for an
/// absolute-form or HTTP/2 request, URI authority) is not in the allowlist.
///
/// A request without `Host` (HTTP/1.0) is served: browsers always send it,
/// so its absence is not a DNS-rebinding attack.
pub async fn check_host(
    State(allowlist): State<Arc<HostAllowlist>>,
    req: Request,
    next: Next,
) -> Response {
    for value in req.headers().get_all(header::HOST) {
        let Ok(value) = value.to_str() else {
            return rejected("Malformed Host header".into());
        };
        if !allowlist.allows(value) {
            return rejected(not_allowed(value));
        }
    }
    if let Some(authority) = req.uri().authority()
        && !allowlist.allows(authority.host())
    {
        return rejected(not_allowed(authority.host()));
    }
    next.run(req).await
}

fn not_allowed(host: &str) -> String {
    format!("Host {host:?} is not allowed (see --allowed-host)")
}

fn rejected(reason: String) -> Response {
    crate::error::couch_error(StatusCode::BAD_REQUEST, "bad_request", &reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowlist(host: &str, extra: &[&str]) -> Option<HostAllowlist> {
        HostAllowlist::from_config(&ServerConfig {
            host: host.into(),
            allowed_hosts: extra.iter().map(|h| h.to_string()).collect(),
            ..Default::default()
        })
    }

    #[test]
    fn loopback_binds_accept_only_loopback_names_and_the_bind_address() {
        for bind in ["127.0.0.1", "localhost", "::1", "[::1]", "127.0.0.2"] {
            let list = allowlist(bind, &[]).expect(bind);
            for ok in [
                "localhost",
                "LOCALHOST:5984",
                "127.0.0.1",
                "127.0.0.1:5984",
                "[::1]",
                "[::1]:5984",
                "[0:0:0:0:0:0:0:1]:80",
                // An empty port is valid (RFC 3986).
                "localhost:",
                "[::1]:",
            ] {
                assert!(list.allows(ok), "{bind}: {ok}");
            }
            for bad in [
                "evil.example",
                "evil.example:5984",
                "localhost.evil.example",
                "127.0.0.1.nip.io",
                "127.0.0.3",
                "::1",
                "::1:5984",
                "[::1",
                "[::1]x",
                "[::1]:x",
                "[localhost]",
                "localhost:abc",
                "localhost:5984:1",
                "",
                ":5984",
                "local host",
                "user@localhost",
            ] {
                assert!(!list.allows(bad), "{bind}: {bad}");
            }
        }
        let list = allowlist("127.0.0.2", &[]).unwrap();
        assert!(list.allows("127.0.0.2:5984"));
    }

    #[test]
    fn allowed_hosts_extend_the_list() {
        let list = allowlist("127.0.0.1", &["db.example.com", "10.0.0.5", "fd00::1"]).unwrap();
        for ok in [
            "db.example.com",
            "DB.Example.COM:443",
            "10.0.0.5:5984",
            "[fd00::1]:5984",
            "localhost",
        ] {
            assert!(list.allows(ok), "{ok}");
        }
        assert!(!list.allows("example.com"));
        assert!(!list.allows("sub.db.example.com"));
    }

    #[test]
    fn non_loopback_binds_check_only_with_allowed_hosts() {
        for bind in ["0.0.0.0", "::", "192.168.1.10", "myhost.local"] {
            assert!(allowlist(bind, &[]).is_none(), "{bind}");
            let list = allowlist(bind, &["db.example.com"]).expect(bind);
            assert!(list.allows("db.example.com:5984"), "{bind}");
            assert!(list.allows("localhost:5984"), "{bind}");
            assert!(list.allows("127.0.0.1"), "{bind}");
            assert!(list.allows("[::1]"), "{bind}");
            assert!(!list.allows("evil.example"), "{bind}");
        }
        let list = allowlist("192.168.1.10", &["db.example.com"]).unwrap();
        assert!(list.allows("192.168.1.10:5984"));
        let list = allowlist("::", &["db.example.com"]).unwrap();
        assert!(list.allows("[::]:5984"));
        assert_eq!(
            list.hosts(),
            ["localhost", "127.0.0.1", "[::1]", "[::]", "db.example.com"]
        );
    }

    #[test]
    fn allowed_host_values_are_validated() {
        assert_eq!(
            parse_allowed_host(" DB.Example.com ").unwrap(),
            "db.example.com"
        );
        assert_eq!(parse_allowed_host("10.0.0.5").unwrap(), "10.0.0.5");
        assert_eq!(parse_allowed_host("fd00::1").unwrap(), "[fd00::1]");
        assert_eq!(parse_allowed_host("[fd00::1]").unwrap(), "[fd00::1]");
        for bad in [
            "",
            "db.example.com:443",
            "http://db.example.com",
            "db.example.com/",
            "[fd00::1]:443",
            "*.example.com",
            "a b",
        ] {
            assert!(parse_allowed_host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn loopback_detection() {
        for host in [
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "[::1]",
            "localhost",
            "LocalHost",
        ] {
            assert!(is_loopback(host), "{host}");
        }
        for host in [
            "0.0.0.0",
            "::",
            "[::]",
            "192.168.1.10",
            "localhost.example",
            "",
        ] {
            assert!(!is_loopback(host), "{host}");
        }
    }
}
