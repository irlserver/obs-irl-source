//! OAuth 2.0 authorization code with PKCE (RFC 7636), refresh, revocation
//! (RFC 7009) and dynamic client registration (RFC 7591).
//!
//! The plugin is a public client: there is no secret to keep, and PKCE is what
//! stops another process on the machine from redeeming a code it intercepted
//! on the loopback redirect.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;

use crate::api;

pub const CLIENT_NAME: &str = "OBS IRL Source";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// 32 random bytes, base64url: a 43 character verifier, the RFC's minimum.
#[must_use]
pub fn new_pkce() -> Pkce {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes::<32>());
    let challenge = challenge_for(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// `BASE64URL(SHA256(verifier))`, RFC 7636 section 4.2.
#[must_use]
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()))
}

/// 128 bits, hex. Only ever compared for equality.
#[must_use]
pub fn nonce() -> String {
    random_bytes::<16>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    // The OS CSPRNG failing to produce 32 bytes is not a condition the plugin
    // can do anything about; a sign-in without a random verifier must not
    // start.
    SystemRandom::new()
        .fill(&mut out)
        .expect("the system random source is unavailable");
    out
}

/// RFC 3986 unreserved characters pass; everything else is `%XX`.
#[must_use]
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub struct AuthorizeRequest<'a> {
    pub endpoint: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub scope: &'a str,
    pub state: &'a str,
    pub code_challenge: &'a str,
}

impl AuthorizeRequest<'_> {
    /// The URL the browser is opened at. Appends to an endpoint that may
    /// already carry a query string.
    #[must_use]
    pub fn url(&self) -> String {
        let sep = if self.endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let params = [
            ("response_type", "code"),
            ("client_id", self.client_id),
            ("redirect_uri", self.redirect_uri),
            ("scope", self.scope),
            ("state", self.state),
            ("code_challenge", self.code_challenge),
            ("code_challenge_method", "S256"),
        ];
        let query: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{k}={}", percent_encode(v)))
            .collect();
        format!("{}{sep}{}", self.endpoint, query.join("&"))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

#[derive(Debug)]
pub enum OAuthError {
    Http(String),
    /// The server answered with an OAuth error body. `invalid_grant` on a
    /// refresh means the session is gone.
    Rejected {
        error: String,
        description: String,
    },
    Json(String),
}

impl OAuthError {
    #[must_use]
    pub fn is_invalid_grant(&self) -> bool {
        matches!(self, Self::Rejected { error, .. } if error == "invalid_grant")
    }
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(e) => write!(f, "{e}"),
            Self::Rejected { error, description } if description.is_empty() => write!(f, "{error}"),
            Self::Rejected { error, description } => write!(f, "{error}: {description}"),
            Self::Json(e) => write!(f, "malformed token response: {e}"),
        }
    }
}

fn token_request(token_endpoint: &str, form: &[(&str, &str)]) -> Result<Tokens, OAuthError> {
    let mut response = api::agent()
        .post(token_endpoint)
        .header("Accept", "application/json")
        .send_form(form.iter().copied())
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    if response.status().as_u16() != 200 {
        let (error, description) = api::read_oauth_error(&mut response);
        return Err(OAuthError::Rejected { error, description });
    }
    response
        .body_mut()
        .read_json()
        .map_err(|e| OAuthError::Json(e.to_string()))
}

pub(crate) fn exchange_code(
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<Tokens, OAuthError> {
    token_request(
        token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", code_verifier),
        ],
    )
}

pub(crate) fn refresh(
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<Tokens, OAuthError> {
    token_request(
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ],
    )
}

/// Best effort: the local state is deleted whether or not this reaches the
/// server.
pub(crate) fn revoke(revocation_endpoint: &str, client_id: &str, token: &str) {
    let _ = api::agent().post(revocation_endpoint).send_form([
        ("token", token),
        ("token_type_hint", "refresh_token"),
        ("client_id", client_id),
    ]);
}

#[derive(serde::Serialize)]
struct Registration<'a> {
    client_name: &'static str,
    redirect_uris: &'a [String],
    token_endpoint_auth_method: &'static str,
    grant_types: [&'static str; 2],
    response_types: [&'static str; 1],
    scope: &'a str,
}

#[derive(Deserialize)]
struct Registered {
    client_id: String,
}

/// RFC 7591, for a provider whose document carries no `client_id`.
pub(crate) fn register_client(
    registration_endpoint: &str,
    redirect_uris: &[String],
    scope: &str,
) -> Result<String, OAuthError> {
    let mut response = api::agent()
        .post(registration_endpoint)
        .header("Accept", "application/json")
        .send_json(Registration {
            client_name: CLIENT_NAME,
            redirect_uris,
            token_endpoint_auth_method: "none",
            grant_types: ["authorization_code", "refresh_token"],
            response_types: ["code"],
            scope,
        })
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    match response.status().as_u16() {
        200 | 201 => {}
        _ => {
            let (error, description) = api::read_oauth_error(&mut response);
            return Err(OAuthError::Rejected { error, description });
        }
    }
    let registered: Registered = response
        .body_mut()
        .read_json()
        .map_err(|e| OAuthError::Json(e.to_string()))?;
    Ok(registered.client_id)
}
