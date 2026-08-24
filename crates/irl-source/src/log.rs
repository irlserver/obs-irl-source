//! `[irl-source]`-prefixed logging over `obs::log::blog`.

pub const PREFIX: &str = "irl-source";

macro_rules! irl_log {
    ($level:expr, $($arg:tt)*) => {
        $crate::log::emit($level, ::std::format_args!($($arg)*))
    };
}

macro_rules! irl_error {
    ($($arg:tt)*) => { irl_log!(::obs::log::Level::Error, $($arg)*) };
}

macro_rules! irl_warn {
    ($($arg:tt)*) => { irl_log!(::obs::log::Level::Warning, $($arg)*) };
}

macro_rules! irl_info {
    ($($arg:tt)*) => { irl_log!(::obs::log::Level::Info, $($arg)*) };
}

#[allow(unused_macros)]
macro_rules! irl_debug {
    ($($arg:tt)*) => { irl_log!(::obs::log::Level::Debug, $($arg)*) };
}

#[doc(hidden)]
pub fn emit(level: obs::log::Level, args: std::fmt::Arguments<'_>) {
    obs::log::blog_prefixed(level, PREFIX, &args.to_string());
}

/// The redacted form of an input URL: protocol, hostname and port only.
/// Paths, userinfo, query parameters and fragments can all contain
/// credentials (SRT passphrases, RTMP stream keys), so they are never copied
/// into the log. Port of the C `irl_log_input_url` (master 706372c).
pub fn redacted_input_url(url: &std::ffi::CStr) -> String {
    let p = ffmpeg::url_split(url);
    if p.protocol.is_empty() {
        return "<redacted>".to_owned();
    }
    let (ob, cb) = if p.hostname.contains(':') { ("[", "]") } else { ("", "") };
    match (!p.hostname.is_empty(), p.port >= 0) {
        (true, true) => format!("{}://{}{}{}:{}", p.protocol, ob, p.hostname, cb, p.port),
        (true, false) => format!("{}://{}{}{}", p.protocol, ob, p.hostname, cb),
        (false, true) => format!("{}://<redacted>:{}", p.protocol, p.port),
        (false, false) => format!("{}://<redacted>", p.protocol),
    }
}

/// `[irl-source] <action>: <redacted url>` at INFO.
pub fn log_input_url(action: &str, url: &std::ffi::CStr) {
    irl_info!("{action}: {}", redacted_input_url(url));
}
