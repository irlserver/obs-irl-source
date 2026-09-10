//! The plugin side of `docs/provider-protocol.md`.
//!
//! A provider is a base URL. This crate discovers it, signs the user in over
//! OAuth 2.0 with a loopback redirect, keeps the refresh token and the last
//! ingest list in a per-provider state file, and resolves one ingest id to a
//! pull URL on request. It knows nothing about libobs: the plugin hands it a
//! state directory, a logger and a "wake the dialogs" callback through
//! [`init`], and everything else is plain Rust over blocking HTTP.
//!
//! Two invariants shape the API:
//!
//! - Nothing here is on the streaming path. The receiver reads the `url`
//!   setting and only that, so a provider that is down, a session that
//!   expired or a plugin that was never signed in cannot stop a stream.
//! - No pull URL is ever written to disk. The list carries names and ids; a
//!   URL exists only between [`resolve`] returning and the caller writing it
//!   into the OBS setting.
//!
//! The pure parts (`catalog`, `discovery`, `oauth`, `loopback`, `store`,
//! `version`) are public so `tests/` can drive them without a network.

#![forbid(unsafe_code)]

pub mod api;
mod browser;
pub mod catalog;
pub mod discovery;
mod hooks;
pub mod loopback;
pub mod oauth;
mod registry;
pub mod store;
pub mod version;

pub use api::Ingest;
pub use catalog::{Catalog, CatalogEntry, CatalogError};
pub use discovery::normalize_base_url;
pub use hooks::{Hooks, Level, init};
pub use registry::{ProviderView, ResolveError, provider_for, refresh, resolve, sign_in, sign_out};
