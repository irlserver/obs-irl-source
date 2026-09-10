//! The provider catalog: which providers the dropdown offers, in what order,
//! and whether a Custom entry is available.
//!
//! It is a file, `providers.json`, in the plugin's data directory next to
//! `locale/`. Every archive ships the stock one (`data/providers.json` in the
//! repo), and a deployment that wants the dropdown to show its own provider
//! and nothing else (a cloud OBS host, say) edits that file rather than
//! forking the build.
//!
//! ```json
//! {
//!   "providers": [
//!     { "name": "Example", "base_url": "https://example.com", "priority": 100 }
//!   ],
//!   "allow_custom": false
//! }
//! ```
//!
//! `priority` defaults to 0 and `allow_custom` to false. Unknown fields are an
//! error rather than ignored: a misspelt `allow_custom` silently defaulting
//! would be worse than a refused file.
//!
//! Reading the file is the plugin's job. This module only parses and
//! validates, so `tests/catalog.rs` can pin the format without a file system.

use std::fmt;

use serde::Deserialize;

use crate::discovery::normalize_base_url;

/// One provider the dropdown offers before any sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The dropdown label until the provider's own document has been read.
    pub name: String,
    /// Where `/.well-known/irl-source-provider.json` is fetched from. Also
    /// what the `provider` setting stores, so it must be unique in a catalog.
    pub base_url: String,
    /// Position in the dropdown: the highest is listed first, ties keep the
    /// order given.
    pub priority: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    providers: Vec<CatalogEntry>,
    allow_custom: bool,
}

impl Catalog {
    /// Highest priority first; ties keep the given order. The entries are
    /// taken as they are; [`Catalog::parse`] is what validates.
    #[must_use]
    pub fn new(mut providers: Vec<CatalogEntry>, allow_custom: bool) -> Catalog {
        providers.sort_by_key(|p| std::cmp::Reverse(p.priority));
        Catalog {
            providers,
            allow_custom,
        }
    }

    /// Parse a `providers.json`. Base URLs are normalized the way the Custom
    /// field's are, so `https://example.com/` and `https://example.com` are
    /// the same provider, and one that normalizes to nothing is refused.
    pub fn parse(json: &str) -> Result<Catalog, CatalogError> {
        let raw: RawCatalog = serde_json::from_str(json)?;
        let mut providers = Vec::with_capacity(raw.providers.len());
        for (index, entry) in raw.providers.into_iter().enumerate() {
            let name = entry.name.trim().to_owned();
            if name.is_empty() {
                return Err(CatalogError::EmptyName { index });
            }
            let Some(base_url) = normalize_base_url(&entry.base_url) else {
                return Err(CatalogError::BadBaseUrl {
                    name,
                    base_url: entry.base_url,
                });
            };
            if providers
                .iter()
                .any(|p: &CatalogEntry| p.base_url == base_url)
            {
                return Err(CatalogError::DuplicateBaseUrl { base_url });
            }
            providers.push(CatalogEntry {
                name,
                base_url,
                priority: entry.priority,
            });
        }
        Ok(Catalog::new(providers, raw.allow_custom))
    }

    /// In dropdown order.
    #[must_use]
    pub fn providers(&self) -> &[CatalogEntry] {
        &self.providers
    }

    /// Whether the dropdown offers a Custom entry with a base URL field.
    #[must_use]
    pub fn allow_custom(&self) -> bool {
        self.allow_custom
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    providers: Vec<RawEntry>,
    #[serde(default)]
    allow_custom: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    name: String,
    base_url: String,
    #[serde(default)]
    priority: u32,
}

#[derive(Debug)]
pub enum CatalogError {
    Json(serde_json::Error),
    /// The entry at `index` (zero-based) has an empty or blank `name`.
    EmptyName {
        index: usize,
    },
    BadBaseUrl {
        name: String,
        base_url: String,
    },
    DuplicateBaseUrl {
        base_url: String,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::Json(e) => write!(f, "not a valid provider catalog: {e}"),
            CatalogError::EmptyName { index } => {
                write!(f, "provider #{} has no name", index + 1)
            }
            CatalogError::BadBaseUrl { name, base_url } => {
                write!(f, "provider {name:?} has an invalid base_url {base_url:?}")
            }
            CatalogError::DuplicateBaseUrl { base_url } => {
                write!(f, "base_url {base_url:?} is listed twice")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<serde_json::Error> for CatalogError {
    fn from(e: serde_json::Error) -> Self {
        CatalogError::Json(e)
    }
}
