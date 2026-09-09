//! The loopback half of the sign-in (RFC 8252 section 7.3).
//!
//! An OBS plugin is a shared library and cannot register a URL scheme, so the
//! authorization server redirects to `http://127.0.0.1:<port>/callback`. The
//! port comes from a fixed list rather than an ephemeral one because most
//! authorization servers match redirect URIs exactly, so every port the plugin
//! might use has to be registered with the client up front.
//!
//! The deadline uses `std::time::Instant`. The plugin's clock rule exists so
//! media and OBS timestamps share one time base; nothing here is a timestamp.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

pub const PORTS: RangeInclusive<u16> = 47420..=47429;
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(120);

#[must_use]
pub fn redirect_uri_for(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

/// Every redirect URI a client registration has to carry.
#[must_use]
pub fn all_redirect_uris() -> Vec<String> {
    PORTS.map(redirect_uri_for).collect()
}

/// Percent-decoding that leaves `+` alone. Query values here are OAuth codes
/// and nonces, which may be base64url and are never form-encoded spaces.
#[must_use]
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The query parameters of an HTTP request line
/// (`GET /callback?code=…&state=… HTTP/1.1`).
#[must_use]
pub fn parse_query(request_line: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(target) = request_line.split_whitespace().nth(1) else {
        return out;
    };
    let Some((_, query)) = target.split_once('?') else {
        return out;
    };
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            out.insert(percent_decode(k), percent_decode(v));
        }
    }
    out
}

/// How a redirect resolves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Code(String),
    /// The server sent `error=…`: the user declined, or the client is not
    /// allowed.
    Denied(String),
    TimedOut,
}

/// Classify one redirect's parameters against the expected `state`.
/// `None` means "not ours": answer the request and keep waiting.
#[must_use]
pub fn classify(params: &HashMap<String, String>, state: &str) -> Option<Outcome> {
    if params.get("state").is_none_or(|s| s != state) {
        return None;
    }
    if let Some(error) = params.get("error") {
        return Some(Outcome::Denied(error.clone()));
    }
    params
        .get("code")
        .filter(|c| !c.is_empty())
        .map(|c| Outcome::Code(c.clone()))
}

pub struct Handoff {
    listener: TcpListener,
    state: String,
}

impl Handoff {
    /// Bind the first free port in [`PORTS`] on the loopback interface only.
    pub fn bind(state: String) -> std::io::Result<Self> {
        let mut last = None;
        for port in PORTS {
            match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))) {
                Ok(listener) => {
                    listener.set_nonblocking(true)?;
                    return Ok(Self { listener, state });
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no loopback port to bind")))
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or_default()
    }

    /// The `state` value the redirect must echo.
    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }

    #[must_use]
    pub fn redirect_uri(&self) -> String {
        redirect_uri_for(self.port())
    }

    /// Accept connections until one carries the expected `state`, or the
    /// deadline passes. Every caller gets an answer, so a stray probe does not
    /// leave a browser tab hanging.
    pub fn wait(&self) -> Outcome {
        let deadline = Instant::now() + SIGN_IN_TIMEOUT;
        while Instant::now() < deadline {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(outcome) = self.serve(stream) {
                        return outcome;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => return Outcome::TimedOut,
            }
        }
        Outcome::TimedOut
    }

    fn serve(&self, mut stream: TcpStream) -> Option<Outcome> {
        stream.set_nonblocking(false).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;

        let mut buf = [0u8; 8192];
        let read = stream.read(&mut buf).ok()?;
        let request = String::from_utf8_lossy(&buf[..read]);
        let line = request.lines().next().unwrap_or_default();
        let outcome = classify(&parse_query(line), &self.state);

        let body = match outcome {
            Some(Outcome::Code(_)) => RESPONSE_OK,
            _ => RESPONSE_BAD,
        };
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        let _ = stream.flush();
        outcome
    }
}

/// `history.replaceState` drops the code out of the address bar the moment
/// the page loads, so a screen capture does not carry it.
const RESPONSE_OK: &str = "<!doctype html><meta charset=utf-8><title>Signed in</title>\
<script>history.replaceState(null,'','/callback')</script>\
<body style=\"font:16px system-ui;padding:3rem;text-align:center\">\
<h1>Signed in</h1><p>You can close this tab and go back to OBS.</p>";

const RESPONSE_BAD: &str = "<!doctype html><meta charset=utf-8><title>Sign-in failed</title>\
<body style=\"font:16px system-ui;padding:3rem;text-align:center\">\
<h1>Sign-in failed</h1><p>Start the sign-in again from OBS.</p>";
