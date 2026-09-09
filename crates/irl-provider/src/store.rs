//! Persisted per-provider state: `<state_dir>/<provider id>.json`.
//!
//! Holds the discovery documents (so a dialog opens without a network call),
//! the client id, the refresh token and the last ingest list. Never a pull
//! URL: the list is names and ids by contract, and the URL the user picked
//! lives only in the OBS setting.
//!
//! Not the OS keyring. `keyring` needs a Secret Service on Linux, which the
//! OBS Flatpak cannot talk to; on macOS a keychain item owned by a plugin
//! inside obs64 prompts for the login password whenever OBS's signature
//! changes, a modal password box mid-stream. A 0600 file is the same trust
//! boundary OBS itself uses for stream keys in `service.json`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::Ingest;
use crate::discovery::{OidcEndpoints, ProviderDoc, valid_provider_id};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Stored {
    pub base_url: String,
    pub doc: ProviderDoc,
    pub oidc: OidcEndpoints,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub ingests: Vec<Ingest>,
}

fn path_for(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Any failure (truncated, half-written by a crash, hand-edited) yields
/// `None` and the provider looks never signed in. Never panics: it runs
/// inside a properties callback, and a panic there unwinds into libobs.
#[must_use]
pub fn parse(raw: &str) -> Option<Stored> {
    let stored: Stored = serde_json::from_str(raw).ok()?;
    valid_provider_id(&stored.doc.id).then_some(stored)
}

/// Every readable state file in `dir`. A file whose name disagrees with the
/// id inside it is skipped: it was not written by [`save`].
pub fn load_all(dir: &Path) -> Vec<Stored> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let stem = path.file_stem()?.to_str()?.to_owned();
            if path.extension()? != "json" {
                return None;
            }
            let stored = parse(&fs::read_to_string(&path).ok()?)?;
            (stored.doc.id == stem).then_some(stored)
        })
        .collect()
}

/// Write atomically: a crash mid-write must not leave a file that reads as
/// "signed in with a truncated token".
pub fn save(dir: &Path, stored: &Stored) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = path_for(dir, &stored.doc.id);
    let json = serde_json::to_string_pretty(stored).map_err(std::io::Error::other)?;

    let tmp = path.with_extension("json.tmp");
    let mut file = create_private(&tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)
}

pub fn remove(dir: &Path, id: &str) {
    let path = path_for(dir, id);
    // The temp file too. A `save` that failed at `rename` left a complete
    // token there, and sign-out exists so the token is gone from the machine.
    let _ = fs::remove_file(path.with_extension("json.tmp"));
    let _ = fs::remove_file(path);
}

#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<fs::File> {
    // Windows inherits the parent directory's ACL, and the OBS plugin_config
    // directory is already under the user's profile.
    fs::File::create(path)
}
