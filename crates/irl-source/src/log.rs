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
