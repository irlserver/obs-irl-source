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
    let (ob, cb) = if p.hostname.contains(':') {
        ("[", "]")
    } else {
        ("", "")
    };
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

/// Query-parameter names whose *value* is a credential.
///
/// [`redacted_log_line`] blanks these wherever they appear, not only inside a
/// URL: FFmpeg prints a bare option string when it fails to parse one
/// (`libavformat/avio.c`: "Error parsing options string %s"), with no scheme
/// in front of it for the URL pass to recognise.
const SENSITIVE_PARAMS: &[&str] = &[
    "passphrase", // SRT
    "streamid",   // SRT — carries the publish key on most ingests
    "secret",     // RIST
    "password",
    "passwd",
    "token",
    "auth",
    "key",
];

/// One line of FFmpeg's own logging, with every credential removed.
///
/// FFmpeg logs whole URLs at warning and error level — `libavformat/libsrt.c`
/// prints `h->filename` for "Connection to %s failed", which is the user's
/// full `srt://host:port?passphrase=…&streamid=…` — so routing its log into
/// the OBS log (see [`route_ffmpeg_log`]) would otherwise undo the redaction
/// [`redacted_input_url`] exists for. Every `scheme://…` token is cut down to
/// the same protocol/host/port form, and the value of any
/// [`SENSITIVE_PARAMS`] key that survives outside a URL is dropped.
pub fn redacted_log_line(line: &str) -> String {
    redact_sensitive_params(&redact_urls(line))
}

/// Rewrite every `scheme://…` token down to `scheme://host[:port]`.
fn redact_urls(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while let Some(rel) = line[i..].find("://") {
        let sep = i + rel;
        // Walk back over the scheme. Every byte accepted here is ASCII, so
        // this cannot land inside a multi-byte character.
        let mut start = sep;
        while start > i && is_scheme_byte(bytes[start - 1]) {
            start -= 1;
        }
        // A scheme is at least one character and starts with a letter; without
        // one this is a bare "://" in prose, not a URL.
        if start == sep || !bytes[start].is_ascii_alphabetic() {
            out.push_str(&line[i..sep + 3]);
            i = sep + 3;
            continue;
        }
        let rest = sep + 3;
        let mut end = rest
            + line[rest..]
                .find(is_url_terminator)
                .unwrap_or(line.len() - rest);
        // FFmpeg ends sentences with the URL in them ("Cannot open connection
        // %s.\n"). Leave that punctuation in the line rather than eating it
        // as part of the URL.
        while end > rest && matches!(bytes[end - 1], b'.' | b',' | b';' | b':' | b'!') {
            end -= 1;
        }
        out.push_str(&line[i..start]);
        out.push_str(&redacted_url_str(&line[start..end]));
        i = end;
    }
    out.push_str(&line[i..]);
    out
}

/// Blank the value of every [`SENSITIVE_PARAMS`] key still in the line.
fn redact_sensitive_params(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while let Some(rel) = line[i..].find('=') {
        let eq = i + rel;
        let mut start = eq;
        while start > i && is_param_byte(bytes[start - 1]) {
            start -= 1;
        }
        // Only a key that starts where a parameter can start, so that the tail
        // of some longer word ("...monkey=") is not mistaken for one.
        let at_boundary = start == 0
            || matches!(
                bytes[start - 1],
                b'?' | b'&' | b';' | b' ' | b'\t' | b'\'' | b'"' | b',' | b'(' | b'['
            );
        out.push_str(&line[i..=eq]);
        i = eq + 1;
        if at_boundary
            && SENSITIVE_PARAMS
                .iter()
                .any(|k| k.eq_ignore_ascii_case(&line[start..eq]))
        {
            out.push_str("<redacted>");
            i += line[i..]
                .find(is_param_terminator)
                .unwrap_or(line.len() - i);
        }
    }
    out.push_str(&line[i..]);
    out
}

/// [`redacted_input_url`] for a URL lifted out of a log line. An interior NUL
/// cannot occur (the line came from a C string), but it is not worth a panic:
/// the whole token goes if one ever does.
fn redacted_url_str(url: &str) -> String {
    match std::ffi::CString::new(url) {
        Ok(c) => redacted_input_url(&c),
        Err(_) => "<redacted>".to_owned(),
    }
}

fn is_scheme_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
}

fn is_param_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

/// Where a URL stops in prose. Quotes matter most: FFmpeg writes `'%s'`.
fn is_url_terminator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '<' | '>' | '|')
}

fn is_param_terminator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '&' | ';' | '\'' | '"' | '`' | ',' | ')' | ']')
}

/// Route the bundled FFmpeg's warnings and errors into the OBS log as
/// `[irl-source] [ffmpeg] <line>`, redacted.
///
/// Port of the C `irl_ffmpeg_log` (master f06d705), which the plugin needs
/// because its FFmpeg is statically linked and hidden behind the module's
/// symbol map: the host's own `av_log` callback cannot see it, and FFmpeg's
/// default one writes to a stderr a Windows OBS does not have.
pub fn route_ffmpeg_log() {
    ffmpeg::log::route_to(|level, line| {
        let level = match level {
            ffmpeg::log::Level::Error => obs::log::Level::Error,
            ffmpeg::log::Level::Warning => obs::log::Level::Warning,
            ffmpeg::log::Level::Info => obs::log::Level::Info,
        };
        irl_log!(level, "[ffmpeg] {}", redacted_log_line(line));
    });
}
