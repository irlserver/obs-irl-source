//! The provider document and the OIDC configuration it points at.

use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::api;

pub const PROTOCOL_VERSION: u32 = 1;
pub const WELL_KNOWN_PATH: &str = "/.well-known/irl-source-provider.json";
pub const OIDC_PATH: &str = "/.well-known/openid-configuration";

/// `{base}/.well-known/irl-source-provider.json`, validated.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderDoc {
    pub protocol_version: u32,
    pub id: String,
    pub name: String,
    pub issuer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default = "default_scope")]
    pub scope: String,
    pub ingests_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_plugin_version: Option<String>,
}

fn default_scope() -> String {
    "openid".to_owned()
}

/// The four entries of `{issuer}/.well-known/openid-configuration` the plugin
/// uses. Everything else in that document is ignored.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct OidcEndpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
}

#[derive(Debug)]
pub enum DiscoveryError {
    Http(String),
    Json(String),
    UnsupportedVersion(u32),
    Invalid(&'static str),
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "malformed document: {e}"),
            Self::UnsupportedVersion(v) => {
                write!(
                    f,
                    "protocol version {v} is not supported (this plugin speaks {PROTOCOL_VERSION})"
                )
            }
            Self::Invalid(what) => write!(f, "document rejected: {what}"),
        }
    }
}

/// Trim, drop a trailing slash, and require a URL the plugin is willing to
/// send a token to: `https://`, or plain `http://` on the loopback interface
/// for a provider under development.
#[must_use]
pub fn normalize_base_url(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return None;
    }
    if trimmed.starts_with("https://") || is_loopback_http(trimmed) {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

fn is_loopback_http(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    // A userinfo section is rejected rather than parsed past: the host in
    // `http://127.0.0.1:1@attacker.example/` is the attacker's, and nothing
    // the plugin talks to needs credentials in the URL.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    // An IPv6 host keeps its brackets here.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[must_use]
pub fn well_known_url(base_url: &str) -> String {
    format!("{}{WELL_KNOWN_PATH}", base_url.trim_end_matches('/'))
}

#[must_use]
pub fn oidc_url(issuer: &str) -> String {
    format!("{}{OIDC_PATH}", issuer.trim_end_matches('/'))
}

/// `^[a-z0-9-]{1,32}$`. The id names a file on disk and prefixes values in
/// the scene collection, so it is kept to characters that are safe in both.
#[must_use]
pub fn valid_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn parse_doc(raw: &str) -> Result<ProviderDoc, DiscoveryError> {
    let doc: ProviderDoc =
        serde_json::from_str(raw).map_err(|e| DiscoveryError::Json(e.to_string()))?;
    if doc.protocol_version != PROTOCOL_VERSION {
        return Err(DiscoveryError::UnsupportedVersion(doc.protocol_version));
    }
    if !valid_provider_id(&doc.id) {
        return Err(DiscoveryError::Invalid("id must match [a-z0-9-]{1,32}"));
    }
    if doc.name.trim().is_empty() {
        return Err(DiscoveryError::Invalid("name is empty"));
    }
    if normalize_base_url(&doc.issuer).is_none() {
        return Err(DiscoveryError::Invalid("issuer must be an https URL"));
    }
    if normalize_base_url(&doc.ingests_endpoint).is_none() {
        return Err(DiscoveryError::Invalid(
            "ingests_endpoint must be an https URL",
        ));
    }
    if doc.scope.trim().is_empty() {
        return Err(DiscoveryError::Invalid("scope is empty"));
    }
    Ok(doc)
}

pub fn parse_oidc(raw: &str) -> Result<OidcEndpoints, DiscoveryError> {
    let oidc: OidcEndpoints =
        serde_json::from_str(raw).map_err(|e| DiscoveryError::Json(e.to_string()))?;
    for (what, url) in [
        ("authorization_endpoint", Some(&oidc.authorization_endpoint)),
        ("token_endpoint", Some(&oidc.token_endpoint)),
        ("registration_endpoint", oidc.registration_endpoint.as_ref()),
        ("revocation_endpoint", oidc.revocation_endpoint.as_ref()),
    ] {
        if let Some(url) = url
            && normalize_base_url(url).is_none()
        {
            return Err(DiscoveryError::Invalid(match what {
                "authorization_endpoint" => "authorization_endpoint must be an https URL",
                "token_endpoint" => "token_endpoint must be an https URL",
                "registration_endpoint" => "registration_endpoint must be an https URL",
                _ => "revocation_endpoint must be an https URL",
            }));
        }
    }
    Ok(oidc)
}

/// Both documents, over the network.
pub(crate) fn fetch(base_url: &str) -> Result<(ProviderDoc, OidcEndpoints), DiscoveryError> {
    let doc = parse_doc(&api::get_text(&well_known_url(base_url)).map_err(DiscoveryError::Http)?)?;
    let oidc = parse_oidc(&api::get_text(&oidc_url(&doc.issuer)).map_err(DiscoveryError::Http)?)?;
    Ok((doc, oidc))
}
