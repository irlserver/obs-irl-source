//! Process-wide provider state, and the workers that change it.
//!
//! Lock discipline: the registry mutex is a leaf. Nothing does I/O or calls a
//! hook while holding it, and it is never taken while a plugin lock is held,
//! so it cannot join the receiver's lock order. Readers clone what they need
//! and drop the guard.
//!
//! Everything that talks to the network runs on a throwaway thread, except
//! [`resolve`], which the properties dialog needs an answer from before its
//! callback returns. The threads are not the receiver's workers: a sign-in
//! outlives any one source, and touches the plugin only through the
//! `wake_dialogs` hook after the state file is written.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::api::{self, ApiError, Ingest, valid_ingest_id};
use crate::discovery::{self, normalize_base_url};
use crate::hooks::{self, log_info, log_warn};
use crate::loopback::{self, Handoff, Outcome};
use crate::store::{self, Stored};
use crate::{browser, oauth, version};

struct Entry {
    stored: Stored,
    /// In memory only. A restart costs one refresh call.
    access_token: Option<String>,
}

#[derive(Default)]
struct Registry {
    by_id: HashMap<String, Entry>,
    /// Base URLs with a browser round-trip in flight, so a second click does
    /// not bind a second port and open a second tab.
    signing_in: HashSet<String>,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| {
        let mut reg = Registry::default();
        if let Some(dir) = &hooks::hooks().state_dir {
            for stored in store::load_all(dir) {
                reg.by_id.insert(
                    stored.doc.id.clone(),
                    Entry {
                        stored,
                        access_token: None,
                    },
                );
            }
        }
        Mutex::new(reg)
    })
}

fn id_for(reg: &Registry, base_url: &str) -> Option<String> {
    reg.by_id
        .values()
        .find(|e| e.stored.base_url == base_url)
        .map(|e| e.stored.doc.id.clone())
}

fn persist(stored: &Stored) {
    if let Some(dir) = &hooks::hooks().state_dir
        && let Err(e) = store::save(dir, stored)
    {
        log_warn!(
            "Could not save the sign-in state for {}: {e}",
            stored.doc.name
        );
    }
}

/// What the properties dialog shows for one provider. Read from cached state;
/// building a dialog never touches the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderView {
    pub id: String,
    pub name: String,
    pub signed_in: bool,
    pub ingests: Vec<Ingest>,
    /// The provider's `min_plugin_version`, when this plugin is below it.
    pub unsupported: Option<String>,
}

/// `None` until the provider at `base_url` has been discovered once.
#[must_use]
pub fn provider_for(base_url: &str) -> Option<ProviderView> {
    let base_url = normalize_base_url(base_url)?;
    let reg = registry().lock();
    let entry = reg.by_id.get(&id_for(&reg, &base_url)?)?;
    let doc = &entry.stored.doc;
    let unsupported = doc
        .min_plugin_version
        .as_deref()
        .filter(|min| !version::at_least(hooks::hooks().plugin_version, min))
        .map(str::to_owned);
    Some(ProviderView {
        id: doc.id.clone(),
        name: doc.name.clone(),
        signed_in: entry.stored.refresh_token.is_some() || entry.access_token.is_some(),
        ingests: entry.stored.ingests.clone(),
        unsupported,
    })
}

/// Releases the sign-in slot however the worker ends, panic included;
/// otherwise one panic would wedge the button for the life of the process.
struct SignInSlot(String);

impl Drop for SignInSlot {
    fn drop(&mut self) {
        registry().lock().signing_in.remove(&self.0);
    }
}

fn spawn(name: &'static str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        log_warn!("Could not start the {name} thread: {e}");
    }
}

/// Discover the provider, open the browser, wait for the redirect, exchange
/// the code, load the list. Returns at once; the work is on a thread.
pub fn sign_in(base_url: &str) {
    let Some(base_url) = normalize_base_url(base_url) else {
        log_warn!("The provider URL must start with https://");
        return;
    };
    if !registry().lock().signing_in.insert(base_url.clone()) {
        log_info!("A sign-in to {base_url} is already in progress; finish it in the browser");
        return;
    }
    let slot = SignInSlot(base_url.clone());
    spawn("irl-provider-signin", move || {
        let _slot = slot;
        run_sign_in(&base_url);
        (hooks::hooks().wake_dialogs)();
    });
}

fn run_sign_in(base_url: &str) {
    let (doc, oidc) = match discovery::fetch(base_url) {
        Ok(d) => d,
        Err(e) => {
            log_warn!("Could not read the provider document at {base_url}: {e}");
            return;
        }
    };
    let name = doc.name.clone();

    // Remember the documents whatever happens next, so the dialog can name
    // the provider and show a version message without a network call.
    let client_id = {
        let mut reg = registry().lock();
        // A provider that changed its id leaves a file under the old one.
        let stale: Vec<String> = reg
            .by_id
            .values()
            .filter(|e| e.stored.base_url == base_url && e.stored.doc.id != doc.id)
            .map(|e| e.stored.doc.id.clone())
            .collect();
        for id in stale {
            reg.by_id.remove(&id);
            if let Some(dir) = &hooks::hooks().state_dir {
                store::remove(dir, &id);
            }
        }
        let previous = reg.by_id.remove(&doc.id);
        let client_id = doc
            .client_id
            .clone()
            .or_else(|| previous.as_ref().and_then(|e| e.stored.client_id.clone()));
        // The session survives the refreshed documents. Every path below can
        // return early (the version gate, a refused registration, a browser
        // that does not open, a sign-in the user abandons), and clearing it
        // here would sign the user out for pressing Sign in a second time.
        // A provider reached at another base URL is another origin: its
        // refresh token must not be replayed to this one.
        let session = previous.filter(|e| e.stored.base_url == base_url);
        reg.by_id.insert(
            doc.id.clone(),
            Entry {
                stored: Stored {
                    base_url: base_url.to_owned(),
                    doc: doc.clone(),
                    oidc: oidc.clone(),
                    client_id: client_id.clone(),
                    refresh_token: session
                        .as_ref()
                        .and_then(|e| e.stored.refresh_token.clone()),
                    ingests: session
                        .as_ref()
                        .map(|e| e.stored.ingests.clone())
                        .unwrap_or_default(),
                },
                access_token: session.and_then(|e| e.access_token),
            },
        );
        client_id
    };

    if let Some(min) = &doc.min_plugin_version
        && !version::at_least(hooks::hooks().plugin_version, min)
    {
        log_warn!(
            "{name} needs plugin version {min} or newer; this is {}",
            hooks::hooks().plugin_version
        );
        persist_entry(&doc.id);
        return;
    }

    let client_id = match client_id {
        Some(id) => id,
        None => match &oidc.registration_endpoint {
            Some(endpoint) => {
                match oauth::register_client(endpoint, &loopback::all_redirect_uris(), &doc.scope) {
                    Ok(id) => id,
                    Err(e) => {
                        log_warn!("{name} refused to register the plugin as a client: {e}");
                        return;
                    }
                }
            }
            None => {
                log_warn!("{name} publishes neither a client_id nor a registration endpoint");
                return;
            }
        },
    };
    set_client_id(&doc.id, &client_id);
    persist_entry(&doc.id);

    let handoff = match Handoff::bind(oauth::nonce()) {
        Ok(h) => h,
        Err(e) => {
            log_warn!("Could not listen for the {name} sign-in on a loopback port: {e}");
            return;
        }
    };
    let pkce = oauth::new_pkce();
    let redirect_uri = handoff.redirect_uri();
    let url = oauth::AuthorizeRequest {
        endpoint: &oidc.authorization_endpoint,
        client_id: &client_id,
        redirect_uri: &redirect_uri,
        scope: &doc.scope,
        state: handoff.state(),
        code_challenge: &pkce.challenge,
    }
    .url();
    if let Err(e) = browser::open(&url) {
        log_warn!("Could not open the browser for the {name} sign-in: {e}");
        return;
    }
    log_info!("Opened the {name} sign-in in your browser");

    let code = match handoff.wait() {
        Outcome::Code(code) => code,
        Outcome::Denied(error) => {
            log_warn!("{name} did not complete the sign-in: {error}");
            return;
        }
        Outcome::TimedOut => {
            log_warn!("The {name} sign-in was not completed");
            return;
        }
    };

    let tokens = match oauth::exchange_code(
        &oidc.token_endpoint,
        &client_id,
        &code,
        &redirect_uri,
        &pkce.verifier,
    ) {
        Ok(t) => t,
        Err(e) => {
            log_warn!("{name} rejected the sign-in code: {e}");
            return;
        }
    };
    if tokens.refresh_token.is_none() {
        log_warn!("{name} issued no refresh token; the sign-in lasts until OBS closes");
    }

    let ingests = match api::list_ingests(&doc.ingests_endpoint, &tokens.access_token) {
        Ok(list) => list,
        Err(ApiError::Unauthorized) => {
            log_warn!("{name} rejected the new session");
            return;
        }
        Err(e) => {
            // The session is good, only this call failed. Refresh picks the
            // list up.
            log_warn!("Signed in to {name}, but could not load the ingest list: {e}");
            Vec::new()
        }
    };

    log_info!("Signed in to {name}; {} ingest(s) available", ingests.len());
    {
        let mut reg = registry().lock();
        if let Some(entry) = reg.by_id.get_mut(&doc.id) {
            entry.stored.refresh_token = tokens.refresh_token;
            entry.stored.ingests = ingests;
            entry.access_token = Some(tokens.access_token);
        }
    }
    persist_entry(&doc.id);
}

fn set_client_id(id: &str, client_id: &str) {
    if let Some(entry) = registry().lock().by_id.get_mut(id) {
        entry.stored.client_id = Some(client_id.to_owned());
    }
}

fn persist_entry(id: &str) {
    let snapshot = registry().lock().by_id.get(id).map(|e| e.stored.clone());
    if let Some(stored) = snapshot {
        persist(&stored);
    }
}

/// Drop the session but keep the documents and the client id, so the next
/// sign-in needs no discovery and no registration.
fn forget_session(id: &str) {
    {
        let mut reg = registry().lock();
        if let Some(entry) = reg.by_id.get_mut(id) {
            entry.access_token = None;
            entry.stored.refresh_token = None;
            entry.stored.ingests.clear();
        }
    }
    persist_entry(id);
}

/// A fresh access token, or `Unauthorized` after the session was forgotten.
fn refresh_access_token(id: &str) -> Result<String, ApiError> {
    let grant = {
        let reg = registry().lock();
        reg.by_id.get(id).and_then(|e| {
            Some((
                e.stored.oidc.token_endpoint.clone(),
                e.stored.client_id.clone()?,
                e.stored.refresh_token.clone()?,
            ))
        })
    };
    let Some((token_endpoint, client_id, refresh_token)) = grant else {
        forget_session(id);
        return Err(ApiError::Unauthorized);
    };
    match oauth::refresh(&token_endpoint, &client_id, &refresh_token) {
        Ok(tokens) => {
            {
                let mut reg = registry().lock();
                if let Some(entry) = reg.by_id.get_mut(id) {
                    entry.access_token = Some(tokens.access_token.clone());
                    if tokens.refresh_token.is_some() {
                        entry.stored.refresh_token = tokens.refresh_token;
                    }
                }
            }
            persist_entry(id);
            Ok(tokens.access_token)
        }
        Err(e) if e.is_invalid_grant() => {
            forget_session(id);
            Err(ApiError::Unauthorized)
        }
        // A provider that is briefly unreachable is not a lost session.
        Err(e) => Err(ApiError::Http(e.to_string())),
    }
}

/// Run `f` with a valid access token, refreshing once on a 401. A 401 with a
/// freshly refreshed token means the session is gone.
fn with_token<T>(id: &str, f: impl Fn(&str) -> Result<T, ApiError>) -> Result<T, ApiError> {
    let current = registry()
        .lock()
        .by_id
        .get(id)
        .and_then(|e| e.access_token.clone());
    let token = match current {
        Some(t) => t,
        None => refresh_access_token(id)?,
    };
    match f(&token) {
        Err(ApiError::Unauthorized) => {
            let fresh = refresh_access_token(id)?;
            match f(&fresh) {
                Err(ApiError::Unauthorized) => {
                    forget_session(id);
                    Err(ApiError::Unauthorized)
                }
                other => other,
            }
        }
        other => other,
    }
}

/// Re-read the ingest list on a thread, then wake the dialogs.
pub fn refresh(base_url: &str) {
    let Some(base_url) = normalize_base_url(base_url) else {
        return;
    };
    spawn("irl-provider-refresh", move || {
        let target = {
            let reg = registry().lock();
            id_for(&reg, &base_url).and_then(|id| {
                let e = reg.by_id.get(&id)?;
                Some((
                    id,
                    e.stored.doc.name.clone(),
                    e.stored.doc.ingests_endpoint.clone(),
                ))
            })
        };
        let Some((id, name, endpoint)) = target else {
            return;
        };
        match with_token(&id, |token| api::list_ingests(&endpoint, token)) {
            Ok(ingests) => {
                log_info!("Loaded {} ingest(s) from {name}", ingests.len());
                if let Some(entry) = registry().lock().by_id.get_mut(&id) {
                    entry.stored.ingests = ingests;
                }
                persist_entry(&id);
            }
            Err(ApiError::Unauthorized) => {
                log_warn!("The {name} session has expired; sign in again")
            }
            Err(e) => log_warn!("Could not load the ingest list from {name}: {e}"),
        }
        (hooks::hooks().wake_dialogs)();
    });
}

/// Revoke the refresh token (best effort) and forget the session.
pub fn sign_out(base_url: &str) {
    let Some(base_url) = normalize_base_url(base_url) else {
        return;
    };
    spawn("irl-provider-signout", move || {
        let target = {
            let reg = registry().lock();
            id_for(&reg, &base_url).and_then(|id| {
                let e = reg.by_id.get(&id)?;
                Some((
                    id,
                    e.stored.doc.name.clone(),
                    e.stored.oidc.revocation_endpoint.clone(),
                    e.stored.client_id.clone(),
                    e.stored.refresh_token.clone(),
                ))
            })
        };
        let Some((id, name, revocation, client_id, refresh_token)) = target else {
            return;
        };
        if let (Some(endpoint), Some(client_id), Some(token)) =
            (revocation, client_id, refresh_token)
        {
            oauth::revoke(&endpoint, &client_id, &token);
        }
        forget_session(&id);
        log_info!("Signed out of {name}");
        (hooks::hooks().wake_dialogs)();
    });
}

#[derive(Debug)]
pub enum ResolveError {
    NotSignedIn,
    Api(ApiError),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSignedIn => f.write_str("not signed in"),
            Self::Api(e) => write!(f, "{e}"),
        }
    }
}

/// The pull URL for one ingest id. Synchronous: it runs inside the dialog's
/// modified callback, which has to write the URL into the settings it was
/// handed before it returns. Bounded by the agent's timeouts, so a dead
/// provider is a pause of a few seconds, and a refresh on the way adds one
/// more.
pub fn resolve(base_url: &str, ingest_id: &str) -> Result<String, ResolveError> {
    let base_url = normalize_base_url(base_url).ok_or(ResolveError::NotSignedIn)?;
    if !valid_ingest_id(ingest_id) {
        return Err(ResolveError::Api(ApiError::NotFound(String::new())));
    }
    let target = {
        let reg = registry().lock();
        id_for(&reg, &base_url).and_then(|id| {
            let e = reg.by_id.get(&id)?;
            let signed_in = e.stored.refresh_token.is_some() || e.access_token.is_some();
            signed_in.then(|| (id, e.stored.doc.ingests_endpoint.clone()))
        })
    };
    let (id, endpoint) = target.ok_or(ResolveError::NotSignedIn)?;
    with_token(&id, |token| api::resolve_url(&endpoint, ingest_id, token))
        .map_err(ResolveError::Api)
}
