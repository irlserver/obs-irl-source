//! obs-irl-source — IRL streaming source plugin for OBS Studio.
//!
//! Module entry points and the source registration. Everything unsafe lives
//! in the `obs` and `ffmpeg` crates; this crate is plain safe Rust.

#![forbid(unsafe_code)]

#[macro_use]
pub mod log;

mod settings;

// Public only so the integration tests under `tests/` (a separate crate) can
// reach them. Nothing here widens what the plugin exposes to OBS: a cdylib
// exports its `#[unsafe(no_mangle)]` items and nothing else.
pub mod audio;
pub mod receiver;
pub mod video;
mod websocket;

pub mod config;
pub mod shared;
pub mod source;

obs::declare_module! {
    module_name: "obs-irl-source",
    default_locale: "en-US",
    // The oldest supported OBS line (build.yml OBS_VERSION). obs_init_module
    // gates on major.minor only, so one binary loads on every newer release.
    api_version: (32, 1, 2),
    name: "IRL Source",
    description: "IRL Source by irlserver.com - live streaming source with jitter buffering, PTS repair, and adaptive latency control",
    author: "Thomas Lekanger",
    load: module_load,
    post_load: module_post_load,
    unload: module_unload,
}

/// Plugin version string (`CARGO_PKG_VERSION`).
pub const PLUGIN_VERSION: &str = env!("IRL_PLUGIN_VERSION");

fn module_load() -> bool {
    install_panic_hook();
    log::route_ffmpeg_log();
    #[cfg(feature = "deadlocks")]
    spawn_deadlock_poller();
    obs::register_source::<source::IrlSource>();
    irl_info!("IRL Source plugin loaded (version {})", PLUGIN_VERSION);
    true
}

fn module_post_load() {
    websocket::register();
}

fn module_unload() {}

#[cfg(feature = "deadlocks")]
fn spawn_deadlock_poller() {
    std::thread::Builder::new()
        .name("irl-deadlock-check".into())
        .spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let deadlocks = parking_lot::deadlock::check_deadlock();
                for (i, threads) in deadlocks.iter().enumerate() {
                    irl_error!("deadlock #{i} detected across {} threads", threads.len());
                    for t in threads {
                        irl_error!("  thread {:?}:\n{:?}", t.thread_id(), t.backtrace());
                    }
                }
            }
        })
        .expect("spawn deadlock poller");
}

/// Every FFI boundary already converts a panic into a log line and a safe
/// return (`obs::panic::guard`, `shared::spawn_worker`), but the payload alone
/// rarely says *where*. The hook runs before unwinding leaves the panic site,
/// so it can capture a backtrace (`force_capture`: no RUST_BACKTRACE needed;
/// release builds keep line tables for it). Installed once; the previous hook
/// is chained so `cargo test`-style consumers keep their output.
fn install_panic_hook() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_owned());
        let backtrace = std::backtrace::Backtrace::force_capture();
        irl_error!("panic at {location}: {info}\nbacktrace:\n{backtrace}");
        previous(info);
    }));
}
