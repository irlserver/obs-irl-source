//! Providers may feed the properties dialog, and nothing else.
//!
//! The feature is worth nothing if a signed-out user, an expired session or an
//! unreachable provider can stop an already-configured source from streaming.
//! The design keeps that true by making the picker write into the existing
//! `url` setting and touch nothing else, so `url` stays the single source of
//! truth and the receiver never learns providers exist.
//!
//! Two tests: one on the configuration itself, and one structural, in the
//! style of `locale_keys.rs`, because the way this invariant would
//! realistically be lost is someone teaching `Config::load` to read a
//! provider key.

use std::ffi::CString;
use std::fs;
use std::path::Path;

use irl_core::{HwDecode, Watermarks, consts};
use obs_irl_source::config::Config;
use obs_irl_source::shared::{HotValues, StreamConfig};

fn url_only(url: &str) -> Config {
    Config {
        stream: StreamConfig {
            url: CString::new(url).unwrap(),
            ffmpeg_options: None,
            hw_decode: HwDecode::Auto,
            low_latency_audio: false,
            small_gap_ms: consts::SMALL_GAP_MS,
            large_gap_ms: consts::LARGE_GAP_MS,
        },
        hot: HotValues {
            reconnect_delay_s: consts::DEFAULT_RECONNECT_DELAY_S as i32,
            adaptive_speed: consts::DEFAULT_ADAPTIVE_SPEED,
            catchup_percent: consts::DEFAULT_CATCHUP_PERCENT as i32,
            wait_for_keyframe: consts::DEFAULT_WAIT_FOR_KEYFRAME,
            clear_on_disconnect: consts::DEFAULT_CLEAR_ON_DISCONNECT,
            watermarks: Watermarks::derive(consts::DEFAULT_BUFFER_TARGET_MS as i32),
        },
        close_when_inactive: consts::DEFAULT_CLOSE_WHEN_INACTIVE,
    }
}

#[test]
fn a_url_alone_is_a_runnable_config() {
    // Nothing else is consulted: no token, no cached list, no provider. This
    // is the scene collection that was saved months ago and still works.
    let config = url_only("srt://relay.example:4000?streamid=play/stream/abc");
    assert!(config.url().is_some());
    assert!(!config.requires_restart(&url_only(
        "srt://relay.example:4000?streamid=play/stream/abc"
    )));
    assert!(config.requires_restart(&url_only(
        "srt://relay.example:4000?streamid=play/stream/def"
    )));
}

/// Every `.rs` under `src/`, relative path and content.
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out
}

#[test]
fn the_streaming_path_does_not_know_about_providers() {
    // If a provider key ever reaches Config, the receiver starts depending on
    // sign-in and an expired session takes a stream off the air. The dialog
    // (`providers.rs`, `settings.rs`) and the module entry point (`lib.rs`,
    // which installs the hooks) are the only places providers may appear.
    let allowed = ["providers.rs", "settings.rs", "lib.rs"];
    let mut leaks = Vec::new();
    for (file, src) in sources() {
        if allowed.contains(&file.as_str()) {
            continue;
        }
        if src.contains("irl_provider") || src.contains("providers::") {
            leaks.push(file);
        }
    }
    assert!(
        leaks.is_empty(),
        "{leaks:?} reference the provider system; the streaming path must stay independent of sign-in"
    );

    let settings = include_str!("../src/settings.rs");
    assert!(
        settings.contains("providers::add_properties"),
        "the properties dialog no longer builds the provider widgets"
    );
    let config = include_str!("../src/config.rs");
    assert!(
        config.contains("get_str(c\"url\")"),
        "Config::load no longer reads the `url` key the picker writes into"
    );
    assert!(
        !config.contains("provider"),
        "Config::load reads a provider key; the streaming path must stay independent of sign-in"
    );
}
