//! The `rouchdb-server` binary: its startup checks and the flags and
//! environment variables of the security options.
//!
//! The servers these tests start listen on 127.0.0.1 only. The refusal tests
//! use 192.0.2.1, a documentation address no machine has: should the check
//! regress, the server fails to bind instead of listening on the network.
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The environment variables of the server's options, removed from every
/// command so that the caller's environment cannot change the tests.
const OPTION_VARS: [&str; 5] = [
    "ROUCHDB_ADMIN",
    "ROUCHDB_CORS_ORIGINS",
    "ROUCHDB_ALLOWED_HOSTS",
    "ROUCHDB_ALLOW_UNAUTHENTICATED",
    "ROUCHDB_TRUST_PROXY",
];

/// Command-line flags plus environment variables.
type Options = (
    &'static [&'static str],
    &'static [(&'static str, &'static str)],
);

fn server(path: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rouchdb-server"));
    for var in OPTION_VARS {
        cmd.env_remove(var);
    }
    cmd.arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Run a command that must exit on its own (killed after a minute).
fn run(cmd: &mut Command) -> Output {
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("the server did not exit: {:?}", child.wait_with_output());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn non_loopback_address_without_admin_is_refused_before_opening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.redb");

    for (flags, env) in [
        (&[][..], None),
        (&[], Some("0")),
        (&[], Some("")),
        (&["--cors-origin", "http://localhost:3000"], Some("false")),
    ] {
        let mut cmd = server(&path);
        cmd.args(["--host", "192.0.2.1", "--port", "0"]).args(flags);
        if let Some(value) = env {
            cmd.env("ROUCHDB_ALLOW_UNAUTHENTICATED", value);
        }
        let output = run(&mut cmd);
        let err = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{flags:?} {env:?}: {err}");
        assert!(
            err.contains("refusing to serve without authentication"),
            "{err}"
        );
        assert!(err.contains("\"192.0.2.1\""), "{err}");
        assert!(err.contains("--admin user:password"), "{err}");
        assert!(err.contains("--allow-unauthenticated"), "{err}");
        assert!(err.contains("ROUCHDB_ALLOW_UNAUTHENTICATED=1"), "{err}");
        // Refused before the file was even opened (so before binding).
        assert!(!path.exists(), "{flags:?} {env:?}");
    }

    // An invalid switch value is a usage error.
    let output = run(server(&path)
        .args(["--host", "192.0.2.1"])
        .env("ROUCHDB_ALLOW_UNAUTHENTICATED", "maybe"));
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(!path.exists());
}

/// With `--admin` or the explicit opt-in the check passes, and the server
/// goes on to open the database (here a path whose directory is missing,
/// so it stops there without binding).
#[test]
fn admin_or_opt_in_passes_the_startup_check() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing").join("db.redb");

    let variants: [Options; 5] = [
        (&["--allow-unauthenticated"], &[]),
        (&[], &[("ROUCHDB_ALLOW_UNAUTHENTICATED", "1")]),
        (&[], &[("ROUCHDB_ALLOW_UNAUTHENTICATED", "TRUE")]),
        (&["--admin", "admin:s3cret"], &[]),
        (&[], &[("ROUCHDB_ADMIN", "admin:s3cret")]),
    ];
    for (flags, env) in variants {
        let mut cmd = server(&path);
        cmd.args(["--host", "192.0.2.1", "--port", "0"])
            .args(flags)
            .envs(env.iter().copied());
        let output = run(&mut cmd);
        let err = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{flags:?} {env:?}: {err}");
        assert!(!err.contains("refusing"), "{flags:?} {env:?}: {err}");
        assert!(
            err.contains("Error opening database"),
            "{flags:?} {env:?}: {err}"
        );
    }

    // A malformed --allowed-host is a usage error.
    let output = run(server(&path).args(["--allowed-host", "db.example.com:443"]));
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("without scheme or port"));
}

/// A running server, killed on drop.
struct Running {
    child: Child,
    base: String,
    client: reqwest::Client,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Running {
    /// Start the server on a free loopback port and wait until it answers.
    async fn start(path: &Path, flags: &[&str], env: &[(&str, &str)]) -> Running {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = server(path)
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .args(flags)
            .envs(env.iter().copied())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut server = Running {
            child,
            base: format!("http://127.0.0.1:{port}"),
            client: reqwest::Client::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if server.client.get(&server.base).send().await.is_ok() {
                return server;
            }
            if let Some(status) = server.child.try_wait().unwrap() {
                let mut err = String::new();
                if let Some(mut pipe) = server.child.stderr.take() {
                    std::io::Read::read_to_string(&mut pipe, &mut err).unwrap();
                }
                panic!("the server exited with {status}: {err}");
            }
            assert!(Instant::now() < deadline, "the server did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> (u16, reqwest::header::HeaderMap, Value) {
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = serde_json::from_str(&resp.text().await.unwrap()).unwrap_or(Value::Null);
        (status, headers, body)
    }

    async fn get_as(&self, host: &str) -> (u16, reqwest::header::HeaderMap, Value) {
        self.send(self.client.get(&self.base).header("host", host))
            .await
    }
}

/// `--allowed-host` / `ROUCHDB_ALLOWED_HOSTS` and `--trust-proxy` /
/// `ROUCHDB_TRUST_PROXY` reach the running server.
#[tokio::test]
async fn security_options_reach_the_running_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.redb");
    let from_flags: Options = (
        &[
            "--admin",
            "admin:s3cret",
            "--allowed-host",
            "db.example.com",
            "--allowed-host",
            "other.example",
            "--trust-proxy",
        ],
        &[],
    );
    let from_env: Options = (
        &[],
        &[
            ("ROUCHDB_ADMIN", "admin:s3cret"),
            ("ROUCHDB_ALLOWED_HOSTS", "db.example.com,other.example"),
            ("ROUCHDB_TRUST_PROXY", "1"),
        ],
    );

    let mut uuids = Vec::new();
    for (flags, env) in [from_flags, from_env] {
        let server = Running::start(&path, flags, env).await;

        for host in ["db.example.com", "other.example:8443", "localhost:5984"] {
            let (status, headers, body) = server.get_as(host).await;
            assert_eq!(status, 200, "{host}: {body}");
            assert_eq!(headers["x-content-type-options"], "nosniff");
            uuids.push(body["uuid"].as_str().unwrap().to_string());
        }
        let (status, headers, body) = server.get_as("rebind.attacker.example").await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"], "bad_request");
        assert_eq!(headers["x-content-type-options"], "nosniff");

        let login = |proto: &'static str| {
            server
                .client
                .post(format!("{}/_session", server.base))
                .header("content-type", "application/json")
                .header("x-forwarded-proto", proto)
                .body(r#"{"name": "admin", "password": "s3cret"}"#)
        };
        let (status, headers, _) = server.send(login("https")).await;
        assert_eq!(status, 200);
        let cookie = headers["set-cookie"].to_str().unwrap();
        assert!(cookie.ends_with("; SameSite=Strict; Secure"), "{cookie}");
        let (_, headers, _) = server.send(login("http")).await;
        let cookie = headers["set-cookie"].to_str().unwrap();
        assert!(cookie.ends_with("; SameSite=Strict"), "{cookie}");
    }
    // Both runs served the same file: one uuid, 32 hex digits.
    uuids.dedup();
    assert_eq!(uuids.len(), 1, "{uuids:?}");
    assert_eq!(uuids[0].len(), 32);
}

/// Without `--trust-proxy` the proxy's header is ignored, and on loopback
/// only the loopback names are served.
#[tokio::test]
async fn defaults_check_loopback_names_and_ignore_the_proxy_header() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::start(
        &dir.path().join("db.redb"),
        &["--admin", "admin:s3cret"],
        &[],
    )
    .await;

    let (status, _, _) = server.get_as("db.example.com").await;
    assert_eq!(status, 400);
    let (status, _, _) = server.get_as("[::1]:5984").await;
    assert_eq!(status, 200);

    let (status, headers, _) = server
        .send(
            server
                .client
                .post(format!("{}/_session", server.base))
                .header("content-type", "application/json")
                .header("x-forwarded-proto", "https")
                .body(r#"{"name": "admin", "password": "s3cret"}"#),
        )
        .await;
    assert_eq!(status, 200);
    let cookie = headers["set-cookie"].to_str().unwrap();
    assert!(cookie.ends_with("; SameSite=Strict"), "{cookie}");
}
