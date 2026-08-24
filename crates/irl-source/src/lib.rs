//! obs-irl-source — IRL streaming source plugin for OBS Studio.
//!
//! Module entry points and the source registration. Everything unsafe lives
//! in the `obs` and `ffmpeg` crates; this crate is plain safe Rust.

#![forbid(unsafe_code)]

#[macro_use]
mod log;

pub mod audio;
mod config;
pub mod receiver;
mod settings;
pub mod shared;
mod source;
mod video;
mod websocket;

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
        .spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let deadlocks = parking_lot::deadlock::check_deadlock();
            for (i, threads) in deadlocks.iter().enumerate() {
                irl_error!("deadlock #{i} detected across {} threads", threads.len());
                for t in threads {
                    irl_error!("  thread {:?}:\n{:?}", t.thread_id(), t.backtrace());
                }
            }
        })
        .expect("spawn deadlock poller");
}
