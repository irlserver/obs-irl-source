//! `blog` bridge. Plugins bind their prefix once with [`log!`]-style macros of
//! their own (see `irl-source/src/log.rs`); this module owns the single
//! `blog(level, "%s", msg)` call and the level constants.

use std::ffi::CString;

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
    // An interior NUL would truncate the line at best and, via CString's
    // error path, drop it entirely. Replacing keeps the message readable and
    // keeps this function infallible, which matters because it is the last
    // thing a panic guard does.
    let c = match CString::new(msg) {
        Ok(c) => c,
        Err(_) => {
            let sanitized: String = msg
                .chars()
                .map(|ch| if ch == '\0' { '?' } else { ch })
                .collect();
            CString::new(sanitized).unwrap_or_else(|_| c"<unprintable log message>".into())
        }
    };

    // SAFETY: `blog` is variadic; the format string is a literal "%s" and the
    // single argument is a NUL-terminated pointer valid for the call.
    unsafe { obs_sys::blog(level as core::ffi::c_int, c"%s".as_ptr(), c.as_ptr()) };
}

/// `blog` with a prefix prepended: `[<prefix>] <msg>`.
pub fn blog_prefixed(level: Level, prefix: &str, msg: &str) {
    blog(level, &format!("[{prefix}] {msg}"));
}
