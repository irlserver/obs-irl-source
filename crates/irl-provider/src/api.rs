//! The two provider endpoints, and the HTTP agent every request goes through.

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::hooks;

/// One entry of the list. `id` is opaque and secret-free by contract; it is
/// what the dropdown stores.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Ingest {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u64>,
}

/// `^[A-Za-z0-9._:-]{1,128}$`: safe in a URL path and in a settings value.
#[must_use]
pub fn valid_ingest_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

#[derive(Debug)]
pub enum ApiError {
    /// 401. The caller refreshes the token once and retries.
    Unauthorized,
    /// 403: visible but not pullable.
    Forbidden(String),
    /// 404: unknown or revoked id.
    NotFound(String),
    Http(String),
    Json(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("not signed in"),
            Self::Forbidden(d) if d.is_empty() => f.write_str("not allowed to pull this ingest"),
            Self::Forbidden(d) => write!(f, "not allowed to pull this ingest: {d}"),
            Self::NotFound(d) if d.is_empty() => f.write_str("ingest not found"),
            Self::NotFound(d) => write!(f, "ingest not found: {d}"),
            Self::Http(e) => f.write_str(e),
            Self::Json(e) => write!(f, "malformed response: {e}"),
        }
    }
}

/// Hard ceilings so a dead provider is a short pause, never a hang. The
/// resolve call runs on the OBS UI thread inside a property callback, so this
/// is what stands between a blackholed DNS lookup and a frozen dialog.
///
/// One agent for the process, so a sign-in and the ingest calls after it reuse
/// the connection instead of paying a TLS handshake each. Built on first use,
/// which is after [`crate::init`] has installed the hooks the user agent comes
/// from.
pub(crate) fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(5)))
            // Inspect status codes ourselves rather than having them raise.
            .http_status_as_error(false)
            .user_agent(hooks::user_agent())
            .build()
            .into()
    })
}

/// An unauthenticated GET returning the body, for the discovery documents.
pub(crate) fn get_text(url: &str) -> Result<String, String> {
    let mut response = agent()
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("HTTP {status} from {url}"));
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}

#[derive(Deserialize, Default)]
struct OAuthErrorBody {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

/// `(error, error_description)` from an OAuth-shaped error body, falling back
/// to the status code when there is no usable body.
pub(crate) fn read_oauth_error(
    response: &mut ureq::http::Response<ureq::Body>,
) -> (String, String) {
    let status = response.status().as_u16();
    let body: OAuthErrorBody = response.body_mut().read_json().unwrap_or_default();
    if body.error.is_empty() {
        (format!("HTTP {status}"), String::new())
    } else {
        (body.error, body.error_description)
    }
}

fn authed_get<T: serde::de::DeserializeOwned>(
    url: &str,
    access_token: &str,
) -> Result<T, ApiError> {
    let mut response = agent()
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {access_token}"))
        .call()
        .map_err(|e| ApiError::Http(e.to_string()))?;
    match response.status().as_u16() {
        200 => response
            .body_mut()
            .read_json()
            .map_err(|e| ApiError::Json(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        403 => Err(ApiError::Forbidden(read_oauth_error(&mut response).1)),
        404 => Err(ApiError::NotFound(read_oauth_error(&mut response).1)),
        other => {
            let (error, description) = read_oauth_error(&mut response);
            Err(ApiError::Http(if error.starts_with("HTTP ") {
                format!("HTTP {other}")
            } else {
                format!("HTTP {other}: {error} {description}")
            }))
        }
    }
}

#[derive(Deserialize)]
struct IngestList {
    #[serde(default)]
    ingests: Vec<Ingest>,
}

/// Entries with an id outside the contract are dropped rather than failing
/// the whole list; the provider is told nothing, the user sees the rest.
#[must_use]
pub fn keep_valid(ingests: Vec<Ingest>) -> Vec<Ingest> {
    ingests
        .into_iter()
        .filter(|i| valid_ingest_id(&i.id) && !i.name.trim().is_empty())
        .collect()
}

pub fn parse_ingest_list(raw: &str) -> Result<Vec<Ingest>, ApiError> {
    let list: IngestList = serde_json::from_str(raw).map_err(|e| ApiError::Json(e.to_string()))?;
    Ok(keep_valid(list.ingests))
}

pub(crate) fn list_ingests(endpoint: &str, access_token: &str) -> Result<Vec<Ingest>, ApiError> {
    let list: IngestList = authed_get(endpoint, access_token)?;
    Ok(keep_valid(list.ingests))
}

#[derive(Deserialize)]
struct Resolved {
    url: String,
}

#[must_use]
pub fn resolve_endpoint(ingests_endpoint: &str, id: &str) -> String {
    format!("{}/{id}/url", ingests_endpoint.trim_end_matches('/'))
}

pub(crate) fn resolve_url(
    ingests_endpoint: &str,
    id: &str,
    access_token: &str,
) -> Result<String, ApiError> {
    let resolved: Resolved = authed_get(&resolve_endpoint(ingests_endpoint, id), access_token)?;
    if resolved.url.trim().is_empty() {
        return Err(ApiError::Json("empty url".to_owned()));
    }
    Ok(resolved.url)
}
