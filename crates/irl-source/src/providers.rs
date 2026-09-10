//! The Provider dropdown, one ingest picker per provider, and the sign-in
//! buttons: the dialog half of `docs/provider-protocol.md`. The protocol
//! itself is `irl_provider`; this file only builds widgets and forwards
//! clicks.
//!
//! Four libobs behaviours dictate the shape, all read out of
//! `shared/properties-view/properties-view.cpp` and `libobs/obs-source.c`:
//!
//! 1. A non-editable list stores the item's *value* while showing its name.
//!    That is what lets an entry read "Main phone · Europe" and store an
//!    ingest id. An editable list would store the displayed text.
//! 2. A non-editable list whose saved value matches no item writes item 0
//!    back into settings on dialog open. Every list here therefore has an
//!    empty-valued item 0, so a stale value cannot clobber anything.
//! 3. Modified callbacks fire on dialog open, not only on a user change. The
//!    ingest picker therefore remembers, next to the pick, the URL it
//!    resolved to: a callback that finds the same pick and the same `url` is
//!    a dialog opening or a widget rebuild, not a choice, and does nothing.
//!    A pick whose `url` has been edited since is stale and resets to item 0.
//! 4. Returning `true` from a callback re-creates the widgets from the
//!    `obs_properties_t` the dialog already holds; only the `update_properties`
//!    signal re-runs the builder, and it has to come from another thread.
//!    Sign-in, refresh and sign-out therefore run on a thread and wake the
//!    dialogs when done ([`wake_dialogs`]).
//!
//! `url` stays the single source of truth. Nothing on the streaming path knows
//! these settings exist; `tests/provider_seam.rs` pins that.
//!
//! Which providers the dropdown lists comes from the [`Catalog`], read once
//! from the `providers.json` shipped in the plugin's data directory
//! ([`load_catalog`]). The repo's `data/providers.json` is the stock list.

use std::ffi::{CStr, CString};
use std::sync::OnceLock;

use irl_core::consts::SOURCE_ID;
use irl_provider::{Catalog, Hooks, Ingest, Level};
use obs::{ClickAction, ComboType, Data, ModifiedAction, Properties, PropertiesRef, TextType};
use parking_lot::Mutex;

use crate::module_text;
use crate::source::IrlSource;

const KEY_PROVIDER: &CStr = c"provider";
const KEY_PROVIDER_URL: &CStr = c"provider_url";
const KEY_URL: &CStr = c"url";
/// `provider`'s value for the Custom entry. Listed entries store their base
/// URL instead, so reordering the list breaks no scene collection.
const VALUE_CUSTOM: &str = "custom";

const PREFIX_INGEST: &str = "provider_ingest";
/// Settings without a widget: the ingest id and the URL the last successful
/// pick in a slot resolved to. [`IngestPicked`] compares against them to tell a
/// user's choice from the callback firing on dialog open.
const PREFIX_RESOLVED_ID: &str = "provider_resolved_id";
const PREFIX_RESOLVED_URL: &str = "provider_resolved_url";
const PREFIX_SIGN_IN: &str = "provider_sign_in";
const PREFIX_SIGN_OUT: &str = "provider_sign_out";
const PREFIX_STATUS: &str = "provider_status";

/// What is in the Custom field right now. A button callback gets no settings,
/// and with `OBS_PROPERTIES_DEFER_UPDATE` the typed URL is not saved until OK,
/// so the field's modified callback mirrors it here. Seeded on dialog open,
/// because that is when modified callbacks first fire.
static CUSTOM_URL: Mutex<String> = Mutex::new(String::new());

/// The provider list, in the plugin's data directory next to `locale/`. Every
/// archive ships the stock one; a deployment edits it. Format and rationale:
/// `irl_provider::catalog`.
const CATALOG_FILE: &CStr = c"providers.json";

static CATALOG: OnceLock<Catalog> = OnceLock::new();

/// Loaded on first use, which [`init`] makes module load so the file is read
/// once, off the dialog's path, and its log line lands with the rest of the
/// module's.
fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(load_catalog)
}

/// A missing or broken `providers.json` leaves the dropdown at Manual URL
/// only, with a warning that names the problem. Like the locale file, it is
/// part of the install, not an optional extra, and streaming does not depend
/// on it either way.
fn load_catalog() -> Catalog {
    let Some(path) = obs::module::data_file(CATALOG_FILE) else {
        irl_warn!(
            "{} is missing from the plugin data directory; the Provider dropdown offers Manual URL only",
            CATALOG_FILE.to_string_lossy()
        );
        return Catalog::new(Vec::new(), false);
    };
    let loaded = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|json| Catalog::parse(&json).map_err(|e| e.to_string()));
    match loaded {
        Ok(catalog) => {
            irl_info!(
                "Provider list loaded from {}: {} provider(s), custom provider {}",
                path.display(),
                catalog.providers().len(),
                if catalog.allow_custom() { "on" } else { "off" }
            );
            catalog
        }
        Err(e) => {
            irl_warn!(
                "Ignoring {}: {e}. The Provider dropdown offers Manual URL only",
                path.display()
            );
            Catalog::new(Vec::new(), false)
        }
    }
}

pub fn init() {
    catalog();
    irl_provider::init(Hooks {
        state_dir: obs::module::config_path(c"providers"),
        plugin_version: crate::PLUGIN_VERSION,
        log,
        wake_dialogs,
    });
}

fn log(level: Level, msg: &str) {
    match level {
        Level::Info => irl_info!("{msg}"),
        Level::Warning => irl_warn!("{msg}"),
    }
}

/// Ask the frontend to reload any open properties dialog on one of our
/// sources. Handles are turned into owned references before anything is
/// signalled: `obs_enum_sources` holds libobs's source list while the callback
/// runs, and re-entering it is not worth the risk for a cosmetic refresh.
fn wake_dialogs() {
    let mut sources = Vec::new();
    obs::source::enum_sources(&mut |source| {
        if source.unversioned_id() == SOURCE_ID
            && let Some(owned) = source.get_ref()
        {
            sources.push(owned);
        }
        true
    });
    for source in &sources {
        source.handle().update_properties();
    }
}

pub fn defaults(settings: &Data<'_>) {
    settings.set_default_str(KEY_PROVIDER, c"");
    settings.set_default_str(KEY_PROVIDER_URL, c"");
}

/// One entry of the Provider dropdown that has ingests: a catalog provider
/// (by its index in the catalog, which is dropdown order) or the Custom
/// field. Manual URL is the absence of a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Listed(usize),
    Custom,
}

impl Slot {
    fn all() -> impl Iterator<Item = Slot> {
        (0..catalog().providers().len())
            .map(Slot::Listed)
            .chain(catalog().allow_custom().then_some(Slot::Custom))
    }

    /// The property id for this slot under `prefix`.
    fn key(self, prefix: &str) -> CString {
        let key = match self {
            Slot::Listed(i) => format!("{prefix}_{i}"),
            Slot::Custom => format!("{prefix}_{VALUE_CUSTOM}"),
        };
        CString::new(key).expect("no NUL in a property id")
    }

    /// The inverse of [`Slot::key`].
    fn parse(name: &CStr, prefix: &str) -> Option<Slot> {
        let rest = name
            .to_str()
            .ok()?
            .strip_prefix(prefix)?
            .strip_prefix('_')?;
        if rest == VALUE_CUSTOM {
            return catalog().allow_custom().then_some(Slot::Custom);
        }
        let i: usize = rest.parse().ok()?;
        (i < catalog().providers().len()).then_some(Slot::Listed(i))
    }

    /// What `provider` holds when this slot is selected.
    fn value(self) -> &'static str {
        match self {
            Slot::Listed(i) => &catalog().providers()[i].base_url,
            Slot::Custom => VALUE_CUSTOM,
        }
    }

    /// The provider base URL, from the settings when the caller has them and
    /// from the mirrored Custom field otherwise.
    fn base_url(self, settings: Option<&Data<'_>>) -> Option<String> {
        match self {
            Slot::Listed(i) => Some(catalog().providers()[i].base_url.clone()),
            Slot::Custom => {
                let typed = match settings {
                    Some(s) => s.get_str(KEY_PROVIDER_URL).unwrap_or_default(),
                    None => CUSTOM_URL.lock().clone(),
                };
                irl_provider::normalize_base_url(&typed)
            }
        }
    }
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).expect("NULs removed")
}

/// Build the dropdown, the pickers and the buttons. Reads only cached state:
/// this runs on the OBS UI thread, and is also reachable from obs-websocket's
/// `GetInputPropertiesListPropertyItems`.
pub fn add_properties(props: &Properties, instance: Option<&IrlSource>) {
    let saved = instance.map(|i| i.handle().settings());
    let saved = saved.as_ref().map(|d| d.data());

    let list = props.add_string_list(KEY_PROVIDER, module_text(c"Provider"), ComboType::List);
    list.add(module_text(c"Provider.Manual"), c"");
    for (i, provider) in catalog().providers().iter().enumerate() {
        list.add(&cstring(&provider.name), &cstring(Slot::Listed(i).value()));
    }
    if catalog().allow_custom() {
        list.add(module_text(c"Provider.Custom"), &cstring(VALUE_CUSTOM));
    }
    list.on_modified::<ProviderChanged>();
    props.add_text(
        c"provider_help",
        module_text(c"ProviderHelp"),
        TextType::Info,
    );
    if catalog().allow_custom() {
        props
            .add_text(
                KEY_PROVIDER_URL,
                module_text(c"ProviderUrl"),
                TextType::Default,
            )
            .on_modified::<ProviderUrlEdited>();
    }

    for slot in Slot::all() {
        // The saved Custom URL wins over the mirror here: on a fresh dialog
        // the mirror is empty until the field's callback fires, which is after
        // this builder has run.
        let base_url = match slot {
            Slot::Custom if saved.is_some() => slot.base_url(saved.as_ref()),
            _ => slot.base_url(None),
        };
        let view = base_url.and_then(|u| irl_provider::provider_for(&u));
        let signed_in = view.as_ref().is_some_and(|v| v.signed_in);
        let unsupported = view.as_ref().and_then(|v| v.unsupported.clone());

        if let Some(min) = &unsupported {
            let text = module_text(c"Provider.TooOld")
                .to_string_lossy()
                .replace("%1", min);
            props.add_text(&slot.key(PREFIX_STATUS), &cstring(&text), TextType::Info);
            continue;
        }
        if signed_in {
            let picker = props.add_string_list(
                &slot.key(PREFIX_INGEST),
                module_text(c"Ingest"),
                ComboType::List,
            );
            picker.add(module_text(c"Ingest.Pick"), c"");
            for ingest in view.iter().flat_map(|v| &v.ingests) {
                picker.add(&cstring(&label(ingest)), &cstring(&ingest.id));
            }
            picker.on_modified::<IngestPicked>();
        }
        props.add_button::<SignInClicked>(
            &slot.key(PREFIX_SIGN_IN),
            if signed_in {
                module_text(c"Ingest.Refresh")
            } else {
                module_text(c"Ingest.SignIn")
            },
        );
        if signed_in {
            props.add_button::<SignOutClicked>(
                &slot.key(PREFIX_SIGN_OUT),
                module_text(c"Ingest.SignOut"),
            );
        }
    }

    let selected = saved
        .and_then(|d| d.get_str(KEY_PROVIDER))
        .unwrap_or_default();
    apply_visibility(&props.view(), &selected);
}

/// `name · detail · live 4.2 Mbps`. Only the pieces the provider sent.
fn label(ingest: &Ingest) -> String {
    let mut out = ingest.name.clone();
    if let Some(detail) = ingest.detail.as_deref().filter(|d| !d.trim().is_empty()) {
        out.push_str(" · ");
        out.push_str(detail);
    }
    match (ingest.online, ingest.bitrate_kbps) {
        (Some(true), Some(kbps)) => {
            out.push_str(" · ");
            out.push_str(&module_text(c"Ingest.Live").to_string_lossy());
            out.push_str(&format!(" {:.1} Mbps", kbps as f64 / 1000.0));
        }
        (Some(true), None) => {
            out.push_str(" · ");
            out.push_str(&module_text(c"Ingest.Live").to_string_lossy());
        }
        (Some(false), _) => {
            out.push_str(" · ");
            out.push_str(&module_text(c"Ingest.Offline").to_string_lossy());
        }
        (None, _) => {}
    }
    out
}

/// Show the selected slot's widgets and hide every other slot's.
fn apply_visibility(props: &PropertiesRef<'_>, selected: &str) {
    if let Some(url) = props.get(KEY_PROVIDER_URL) {
        url.set_visible(selected == VALUE_CUSTOM);
    }
    for slot in Slot::all() {
        let visible = slot.value() == selected;
        for prefix in [
            PREFIX_STATUS,
            PREFIX_INGEST,
            PREFIX_SIGN_IN,
            PREFIX_SIGN_OUT,
        ] {
            if let Some(property) = props.get(&slot.key(prefix)) {
                property.set_visible(visible);
            }
        }
    }
}

struct ProviderChanged;

impl ModifiedAction for ProviderChanged {
    fn modified(_: &CStr, props: &PropertiesRef<'_>, settings: &Data<'_>) -> bool {
        let selected = settings.get_str(KEY_PROVIDER).unwrap_or_default();
        apply_visibility(props, &selected);
        true
    }
}

struct ProviderUrlEdited;

impl ModifiedAction for ProviderUrlEdited {
    fn modified(_: &CStr, _: &PropertiesRef<'_>, settings: &Data<'_>) -> bool {
        *CUSTOM_URL.lock() = settings.get_str(KEY_PROVIDER_URL).unwrap_or_default();
        false
    }
}

/// Picking an ingest resolves its URL and writes it into `url`. Synchronous,
/// on the UI thread: the settings object is only ours for the duration of the
/// callback, and the provider client's timeouts bound the wait.
///
/// The pick stays in the settings so the dialog reopens on it. What makes that
/// safe is the resolved pair remembered next to it: this callback fires on
/// every dialog open and widget rebuild, and those must neither hit the
/// network nor overwrite a `url` the user has edited since.
struct IngestPicked;

impl ModifiedAction for IngestPicked {
    fn modified(name: &CStr, _: &PropertiesRef<'_>, settings: &Data<'_>) -> bool {
        let Some(slot) = Slot::parse(name, PREFIX_INGEST) else {
            return false;
        };
        // `get_str` is `None` for the empty string, so item 0 and the resets
        // below land here and do nothing. That is also what stops the rebuild
        // this returns `true` for from looping.
        let Some(pick) = settings.get_str(name) else {
            return false;
        };
        let id_key = slot.key(PREFIX_RESOLVED_ID);
        let url_key = slot.key(PREFIX_RESOLVED_URL);
        if settings.get_str(&id_key).as_deref() == Some(pick.as_str()) {
            if settings.get_str(&url_key) == settings.get_str(KEY_URL) {
                return false;
            }
            // `url` is no longer what this pick produced: edited by hand, or
            // written by another provider's picker. Forgetting the pair is
            // what lets the same ingest be picked again afterwards.
            settings.set_str(name, c"");
            settings.set_str(&id_key, c"");
            settings.set_str(&url_key, c"");
            return true;
        }
        let resolved =
            slot.base_url(Some(settings)).and_then(|base_url| {
                match irl_provider::resolve(&base_url, &pick) {
                    Ok(url) => match CString::new(url) {
                        Ok(url) => Some(url),
                        Err(_) => {
                            irl_warn!("The provider returned a URL with a NUL in it");
                            None
                        }
                    },
                    Err(e) => {
                        irl_warn!("Could not resolve the selected ingest: {e}");
                        None
                    }
                }
            });
        match resolved {
            Some(url) => {
                crate::log::log_input_url("Provider ingest selected", &url);
                settings.set_str(KEY_URL, &url);
                settings.set_str(&id_key, &cstring(&pick));
                settings.set_str(&url_key, &url);
            }
            None => settings.set_str(name, c""),
        }
        true
    }
}

/// Sign in, or refresh the list once signed in. Returns `false` and does the
/// work on a thread: `true` would only rebuild the widgets from the list the
/// dialog was opened with, and the wake-up that re-runs the builder has to be
/// raised off the UI thread (see the module doc).
struct SignInClicked;

impl ClickAction for SignInClicked {
    fn clicked(name: &CStr) -> bool {
        let Some(slot) = Slot::parse(name, PREFIX_SIGN_IN) else {
            return false;
        };
        let Some(base_url) = slot.base_url(None) else {
            irl_warn!("Enter the provider URL (https://…) before signing in");
            return false;
        };
        let signed_in = irl_provider::provider_for(&base_url).is_some_and(|v| v.signed_in);
        if signed_in {
            irl_provider::refresh(&base_url);
        } else {
            irl_provider::sign_in(&base_url);
        }
        false
    }
}

struct SignOutClicked;

impl ClickAction for SignOutClicked {
    fn clicked(name: &CStr) -> bool {
        if let Some(slot) = Slot::parse(name, PREFIX_SIGN_OUT)
            && let Some(base_url) = slot.base_url(None)
        {
            irl_provider::sign_out(&base_url);
        }
        false
    }
}
