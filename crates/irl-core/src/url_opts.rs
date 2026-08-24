//! Demuxer option table (port of `apply_demuxer_options`, `receiver-stream.c:20-187`).

use std::borrow::Cow;

/// Whether `url` starts with `scheme://` (prefix match, not substring).
pub fn has_scheme(url: &str, scheme: &str) -> bool {
    let _ = (url, scheme);
    todo!("W1-C")
}

/// The ordered `(key, value)` list to feed `av_dict_set`; later entries
/// override earlier ones, and the user's `extra` options come last.
pub fn demuxer_options(url: &str, extra: Option<&str>, network_buffer_mb: i64, fast_probe: bool) -> Vec<(Cow<'static, str>, Cow<'static, str>)> {
    let _ = (url, extra, network_buffer_mb, fast_probe);
    todo!("W1-C")
}

/// Parse space-separated `key=value` pairs; entries without `=` are ignored.
pub fn parse_extra(extra: &str) -> Vec<(String, String)> {
    let _ = extra;
    todo!("W1-C")
}
