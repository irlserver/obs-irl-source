//! `blog` bridge. Plugins bind their prefix once with [`log!`]-style macros of
//! their own (see `irl-source/src/log.rs`); this module owns the single
//! `blog(level, "%s", msg)` call and the level constants.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Level {
    Error = obs_sys::LOG_ERROR,
    Warning = obs_sys::LOG_WARNING,
    Info = obs_sys::LOG_INFO,
    Debug = obs_sys::LOG_DEBUG,
}

/// Emit one log line through libobs. `msg` is passed as the `%s` argument, so
/// it may contain anything (including `%`).
pub fn blog(level: Level, msg: &str) {
    let _ = (level, msg);
    todo!("W1-A: CString + obs_sys::blog(level, c\"%s\", msg)")
}

/// `blog` with a prefix prepended: `[<prefix>] <msg>`.
pub fn blog_prefixed(level: Level, prefix: &str, msg: &str) {
    let _ = (level, prefix, msg);
    todo!("W1-A")
}
