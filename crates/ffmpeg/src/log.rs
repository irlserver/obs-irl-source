//! The bundled FFmpeg's diagnostics, routed to a caller-supplied sink.
//!
//! Without this the plugin's media stack is silent. The FFmpeg it links is
//! static and hidden behind the module's symbol map, so the host OBS's own
//! `av_log` callback can never see it, and FFmpeg's default callback writes to
//! a stderr a Windows OBS does not have. Every libavformat/libsrt failure —
//! the handshake error, the "no TS sync" probe warning, the reason a URL would
//! not open — went nowhere at all. Port of the C `irl_ffmpeg_log`
//! (master f06d705).
//!
//! The sink receives a *formatted* line and nothing else: this module owns the
//! `va_list`, and the caller never sees FFmpeg's format string or its
//! arguments. What the sink must still do is redact it — FFmpeg prints whole
//! URLs (`libsrt.c`: "Connection to %s failed", with `h->filename`), and those
//! carry SRT passphrases and RTMP stream keys.

use core::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

use ffmpeg_sys_next as sys;

/// How FFmpeg's numeric levels collapse onto what a host log wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// `AV_LOG_ERROR` and worse (`FATAL`, `PANIC`).
    Error,
    /// `AV_LOG_WARNING`.
    Warning,
    /// `AV_LOG_INFO` and below in importance.
    Info,
}

/// A formatted, newline-stripped FFmpeg log line and its level.
///
/// Non-capturing: FFmpeg calls it from whatever thread logged, including
/// decoder worker threads, so it must be thread safe.
pub type Sink = fn(Level, &str);

static SINK: OnceLock<Sink> = OnceLock::new();

/// The `va_list` bindgen resolved for this target, as it appears in the
/// `av_log_set_callback` / `av_log_format_line2` signatures.
///
/// A wrong guess here cannot become a silent ABI mismatch: [`trampoline`] is
/// passed straight to `av_log_set_callback` and forwards straight to
/// `av_log_format_line2`, so both uses are type-checked against the bindings
/// and a new target with a different representation fails to compile.
#[cfg(all(target_arch = "x86_64", not(target_env = "msvc")))]
type VaList = *mut sys::__va_list_tag;
#[cfg(not(all(target_arch = "x86_64", not(target_env = "msvc"))))]
type VaList = sys::va_list;

/// Send FFmpeg's warnings and errors to `sink`.
///
/// `AV_LOG_INFO`, FFmpeg's default, is per-frame chatter from the decoders and
/// would bury the host log on a long stream, so the level is pinned to
/// `AV_LOG_WARNING`. Callable once per process (FFmpeg's callback is global);
/// later calls are ignored and report `false`.
pub fn route_to(sink: Sink) -> bool {
    if SINK.set(sink).is_err() {
        return false;
    }
    // SAFETY: both set a global in this crate's statically linked libavutil.
    // FFmpeg documents the callback as being replaceable at any time, and
    // `trampoline` is valid for the life of the process.
    unsafe {
        sys::av_log_set_level(sys::AV_LOG_WARNING);
        sys::av_log_set_callback(Some(trampoline));
    }
    true
}

/// The longest line handed to the sink. FFmpeg's own default callback uses
/// 1024 too; anything past it is truncated rather than split.
const LINE_MAX: usize = 1024;

unsafe extern "C" fn trampoline(avcl: *mut c_void, level: c_int, fmt: *const c_char, vl: VaList) {
    // A panic must not unwind into FFmpeg's C frame. There is nowhere to
    // report it — the sink is the log — so it is swallowed; the sink's own
    // FFI boundary logs whatever it can.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `avcl`, `fmt` and `vl` are FFmpeg's own arguments, valid for
        // this call; `vl` is consumed exactly once. `line` is a writable
        // buffer of the length passed, which av_log_format_line2 NUL
        // terminates inside.
        unsafe { format_line(avcl, level, fmt, vl) };
    }));
}

/// # Safety
///
/// Callable only from [`trampoline`], with FFmpeg's own callback arguments.
unsafe fn format_line(avcl: *mut c_void, level: c_int, fmt: *const c_char, vl: VaList) {
    let Some(sink) = SINK.get() else {
        return;
    };
    // SAFETY: reads a global int.
    if level > unsafe { sys::av_log_get_level() } {
        return;
    }

    let mut line = [0 as c_char; LINE_MAX];
    // av_log_format_line2 wants a persistent int, but only so a continuation
    // line (FFmpeg emitting a message in several calls) can suppress the
    // repeated prefix. Each partial still reads fine as its own entry, and a
    // shared one would have to be synchronised across threads.
    let mut print_prefix: c_int = 1;
    // SAFETY: per the contract above; the buffer is `LINE_MAX` long and that
    // length is what is passed.
    unsafe {
        sys::av_log_format_line2(
            avcl,
            level,
            fmt,
            vl,
            line.as_mut_ptr(),
            LINE_MAX as c_int,
            &mut print_prefix,
        );
    }

    // SAFETY: av_log_format_line2 always NUL terminates within the buffer
    // (truncating if it must), so the string ends inside `line`.
    let text = unsafe { CStr::from_ptr(line.as_ptr()) }.to_string_lossy();
    // FFmpeg terminates its lines; a host log adds its own newline.
    let text = text.trim_end_matches(['\n', '\r']);
    if text.is_empty() {
        return;
    }
    sink(level_of(level), text);
}

fn level_of(level: c_int) -> Level {
    if level <= sys::AV_LOG_ERROR {
        Level::Error
    } else if level <= sys::AV_LOG_WARNING {
        Level::Warning
    } else {
        Level::Info
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn levels_collapse_the_way_the_c_plugin_did() {
        assert_eq!(level_of(sys::AV_LOG_PANIC), Level::Error);
        assert_eq!(level_of(sys::AV_LOG_FATAL), Level::Error);
        assert_eq!(level_of(sys::AV_LOG_ERROR), Level::Error);
        assert_eq!(level_of(sys::AV_LOG_WARNING), Level::Warning);
        assert_eq!(level_of(sys::AV_LOG_INFO), Level::Info);
        assert_eq!(level_of(sys::AV_LOG_DEBUG), Level::Info);
    }

    /// The whole FFI path in one go: `av_log`'s varargs through the
    /// trampoline, the `va_list` alias, the level pin and the newline strip.
    /// The line arrives *unredacted* — that is the sink's job, and this pins
    /// the contract that says so.
    #[test]
    fn a_routed_line_reaches_the_sink_formatted() {
        static SEEN: Mutex<Vec<(Level, String)>> = Mutex::new(Vec::new());
        fn sink(level: Level, line: &str) {
            SEEN.lock().unwrap().push((level, line.to_owned()));
        }
        assert!(route_to(sink));
        assert!(!route_to(sink), "FFmpeg's callback is global: set once");

        // SAFETY: av_log is variadic; both format strings are C literals and
        // the arguments match their conversions exactly.
        unsafe {
            sys::av_log(
                core::ptr::null_mut(),
                sys::AV_LOG_ERROR,
                c"Connection to %s failed\n".as_ptr(),
                c"srt://ingest.example:9000?passphrase=hunter2".as_ptr(),
            );
            sys::av_log(
                core::ptr::null_mut(),
                sys::AV_LOG_INFO,
                c"chatter\n".as_ptr(),
            );
        }

        // Other tests in this binary share the process, and the sink with it.
        let seen = SEEN.lock().unwrap();
        let routed: Vec<_> = seen
            .iter()
            .filter(|(_, l)| l.starts_with("Connection"))
            .collect();
        assert_eq!(routed.len(), 1, "got {seen:?}");
        assert_eq!(
            routed[0],
            &(
                Level::Error,
                "Connection to srt://ingest.example:9000?passphrase=hunter2 failed".to_owned()
            )
        );
        assert!(
            !seen.iter().any(|(_, l)| l == "chatter"),
            "AV_LOG_INFO must not survive the level pin: {seen:?}"
        );
    }
}
