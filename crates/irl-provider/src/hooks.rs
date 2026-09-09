//! What the plugin supplies: where state lives, how to log, how to wake the
//! properties dialogs.
//!
//! A struct of plain function pointers rather than a trait object so the
//! crate has nothing to hold on to and the plugin nothing to keep alive.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Hooks {
    /// Directory for the per-provider state files. `None` disables
    /// persistence; sign-ins then last until the process exits.
    pub state_dir: Option<PathBuf>,
    /// Goes into `User-Agent` and is compared against `min_plugin_version`.
    pub plugin_version: &'static str,
    pub log: fn(Level, &str),
    /// Called from a worker thread after a sign-in, refresh or sign-out
    /// changed what the properties dialog should show.
    pub wake_dialogs: fn(),
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

/// Install the hooks. A second call is ignored: the first caller owns them.
pub fn init(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}

fn noop_log(_: Level, _: &str) {}
fn noop_wake() {}

static UNINITIALISED: Hooks = Hooks {
    state_dir: None,
    plugin_version: "0.0.0",
    log: noop_log,
    wake_dialogs: noop_wake,
};

pub(crate) fn hooks() -> &'static Hooks {
    HOOKS.get().unwrap_or(&UNINITIALISED)
}

pub(crate) fn user_agent() -> String {
    format!("obs-irl-source/{}", hooks().plugin_version)
}

macro_rules! log_info {
    ($($arg:tt)*) => {
        ($crate::hooks::hooks().log)($crate::hooks::Level::Info, &format!($($arg)*))
    };
}

macro_rules! log_warn {
    ($($arg:tt)*) => {
        ($crate::hooks::hooks().log)($crate::hooks::Level::Warning, &format!($($arg)*))
    };
}

pub(crate) use {log_info, log_warn};
