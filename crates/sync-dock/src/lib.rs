//! The IRL Sync dock, behind a Rust trait.
//!
//! The dock is Spillmaker's Qt widget from the C plugin (`cxx/sync-dock.cpp`),
//! compiled in by `build.rs` when Qt6 is found and stubbed out otherwise. It
//! talks to the plugin through a table of C function pointers
//! (`cxx/irl-sync-dock.h`); this crate owns that boundary — the only unsafe
//! code the dock needs — and exposes it as [`DockBackend`], which the plugin
//! implements in safe Rust.
//!
//! Like `obs` and `ffmpeg`, this is an FFI crate: every raw pointer the dock
//! touches lives here, and the plugin crate keeps `#![forbid(unsafe_code)]`.

#![deny(missing_docs)]

/// `IRL_SYNC_OFF`.
pub const STATUS_OFF: i32 = 0;
/// `IRL_SYNC_NO_TIMECODE`.
pub const STATUS_NO_TIMECODE: i32 = 1;
/// `IRL_SYNC_STALE`.
pub const STATUS_STALE: i32 = 2;
/// `IRL_SYNC_TOO_SLOW`.
pub const STATUS_TOO_SLOW: i32 = 3;
/// `IRL_SYNC_ACQUIRING`.
pub const STATUS_ACQUIRING: i32 = 4;
/// `IRL_SYNC_LOCKED`.
pub const STATUS_LOCKED: i32 = 5;

/// `IRL_SYNC_TC_OK`.
pub const REASON_OK: i32 = 0;
/// `IRL_SYNC_TC_NO_CLOCK`.
pub const REASON_NO_CLOCK: i32 = 1;
/// `IRL_SYNC_TC_CODEC`.
pub const REASON_CODEC: i32 = 2;
/// `IRL_SYNC_TC_ABSENT`.
pub const REASON_ABSENT: i32 = 3;

/// Sources the dock can list (`IRL_SYNC_MAX_SOURCES`).
pub const MAX_SOURCES: usize = 64;

/// One timecode as the dock displays it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DockTimecode {
    /// Hours.
    pub hours: u8,
    /// Minutes.
    pub minutes: u8,
    /// Seconds.
    pub seconds: u8,
    /// Frame index within the second.
    pub n_frames: u16,
}

/// One source's status, with the codes above.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DockSnapshot {
    /// One of the `STATUS_*` codes.
    pub status: i32,
    /// One of the `REASON_*` codes.
    pub tc_reason: i32,
    /// The latest timecode, if one is on display.
    pub timecode: Option<DockTimecode>,
    /// Arrival latency.
    pub latency_ms: i64,
    /// Rolling peak of the arrival latency.
    pub latency_peak_ms: i64,
    /// Hold applied by the delay line.
    pub added_ms: i64,
    /// Presentation error.
    pub error_ms: i64,
    /// Offset that would clear the peak.
    pub required_offset_ms: i64,
}

/// One registered source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockEntry {
    /// The source's display name.
    pub source_name: String,
    /// Whether it is ticked for sync.
    pub sync_enabled: bool,
    /// Its published status.
    pub snap: DockSnapshot,
}

/// The three global settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockConfig {
    /// The master switch.
    pub enabled: bool,
    /// The shared offset.
    pub offset_ms: i32,
    /// The NTP server.
    pub ntp_server: String,
}

/// The NTP reference as the dock shows it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DockNtpStatus {
    /// A sample has been accepted.
    pub synced: bool,
    /// Local-to-UTC offset.
    pub offset_ns: i64,
    /// Round trip of the current sample.
    pub rtt_ns: i64,
    /// Age of the current sample.
    pub age_ns: u64,
    /// Server being polled.
    pub server: String,
    /// Server configured.
    pub primary: String,
    /// Burst policy in force.
    pub burst_mode: bool,
    /// Failed over to the pool.
    pub fallback_active: bool,
    /// The drift fit is valid.
    pub drift_valid: bool,
    /// Crystal error, positive when the local clock runs fast.
    pub drift_ppm: f64,
}

/// The bounds the dock's controls are built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DockLimits {
    /// Offset floor.
    pub min_offset_ms: i32,
    /// Offset ceiling.
    pub max_offset_ms: i32,
    /// Offset step.
    pub offset_step_ms: i32,
    /// The default server, shown as the placeholder.
    pub default_ntp_server: &'static str,
}

/// Where a dock log line goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Informational.
    Info,
    /// Warning.
    Warning,
    /// Error.
    Error,
}

/// What the dock reads and writes. Implemented by the plugin; every method may
/// be called from the Qt UI thread.
pub trait DockBackend: Send + Sync {
    /// Control bounds.
    fn limits(&self) -> DockLimits;
    /// The current settings.
    fn config(&self) -> DockConfig;
    /// The master switch was toggled.
    fn set_enabled(&self, enabled: bool);
    /// The offset was set (already a whole number of steps).
    fn set_offset_ms(&self, offset_ms: i32);
    /// The server was changed.
    fn set_ntp_server(&self, host: &str);
    /// The reference clock.
    fn ntp_status(&self) -> DockNtpStatus;
    /// UTC now per the reference, or `None` while unsynced.
    fn utc_now_ns(&self) -> Option<i64>;
    /// Every registered source.
    fn collect(&self) -> Vec<DockEntry>;
    /// A line for the OBS log.
    fn log(&self, level: LogLevel, message: &str);
}

/// Whether this build carries the dock at all.
pub const fn available() -> bool {
    cfg!(irl_dock)
}

#[cfg(irl_dock)]
pub use ffi::{register, unregister};

/// Register the dock with the OBS frontend. In a build without Qt this is a
/// no-op returning `false`.
#[cfg(not(irl_dock))]
pub fn register(_backend: &'static dyn DockBackend) -> bool {
    false
}

/// Remove the dock. No-op in a build without Qt.
#[cfg(not(irl_dock))]
pub fn unregister() {}

#[cfg(irl_dock)]
mod ffi {
    //! The C ABI side, mirroring `cxx/irl-sync-dock.h` field for field.

    use core::ffi::{CStr, c_char, c_int, c_void};
    use std::ffi::CString;
    use std::sync::OnceLock;

    use super::{DockBackend, DockEntry, LogLevel, MAX_SOURCES};

    const ABI_VERSION: u32 = 1;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Timecode {
        hours: u8,
        minutes: u8,
        seconds: u8,
        n_frames: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Snapshot {
        status: c_int,
        tc_reason: c_int,
        have_timecode: bool,
        tc: Timecode,
        latency_ms: i64,
        latency_peak_ms: i64,
        added_ms: i64,
        error_ms: i64,
        required_offset_ms: i64,
    }

    #[repr(C)]
    struct Entry {
        source_name: [c_char; 256],
        sync_enabled: bool,
        snap: Snapshot,
    }

    #[repr(C)]
    struct Config {
        enabled: bool,
        offset_ms: i32,
        ntp_server: [c_char; 128],
    }

    #[repr(C)]
    struct NtpStatus {
        synced: bool,
        offset_ns: i64,
        rtt_ns: i64,
        age_ns: u64,
        server: [c_char; 128],
        primary: [c_char; 128],
        burst_mode: bool,
        fallback_active: bool,
        drift_valid: bool,
        drift_ppm: f64,
    }

    #[repr(C)]
    struct Api {
        abi_version: u32,
        min_offset_ms: i32,
        max_offset_ms: i32,
        offset_step_ms: i32,
        default_ntp_server: *const c_char,
        config_get: unsafe extern "C" fn(*mut Config),
        set_enabled: unsafe extern "C" fn(bool),
        set_offset_ms: unsafe extern "C" fn(i32),
        set_ntp_server: unsafe extern "C" fn(*const c_char),
        ntp_status: unsafe extern "C" fn(*mut NtpStatus),
        utc_now_ns: unsafe extern "C" fn(*mut i64) -> bool,
        collect: unsafe extern "C" fn(*mut Entry, usize) -> usize,
        log: Option<unsafe extern "C" fn(c_int, *const c_char)>,
    }

    #[repr(C)]
    #[derive(Default)]
    struct AbiSizes {
        timecode: usize,
        snapshot: usize,
        entry: usize,
        config: usize,
        ntp_status: usize,
        api: usize,
    }

    unsafe extern "C" {
        fn irl_sync_dock_abi_sizes(out: *mut AbiSizes);
        fn irl_sync_dock_register(api: *const Api) -> bool;
        fn irl_sync_dock_unregister();
    }

    /// The backend, installed once by [`register`]. The dock calls back on
    /// the Qt thread for as long as the process lives.
    static BACKEND: OnceLock<&'static dyn DockBackend> = OnceLock::new();
    /// The default server string handed to C++, kept alive for the process.
    static DEFAULT_SERVER: OnceLock<CString> = OnceLock::new();

    fn backend() -> Option<&'static dyn DockBackend> {
        BACKEND.get().copied()
    }

    /// Copy `s` into a fixed C buffer, NUL-terminated and truncated, as
    /// `snprintf(buf, sizeof buf, "%s", s)` would.
    fn copy_str(dst: &mut [c_char], s: &str) {
        let bytes = s.as_bytes();
        let n = bytes.len().min(dst.len().saturating_sub(1));
        for (d, &b) in dst.iter_mut().zip(&bytes[..n]) {
            *d = b as c_char;
        }
        if let Some(last) = dst.get_mut(n) {
            *last = 0;
        }
    }

    /// Borrow a C string the dock hands in for the duration of a call.
    ///
    /// # Safety
    /// `p` is NULL or a NUL-terminated string valid for the call.
    unsafe fn arg_str<'a>(p: *const c_char) -> &'a str {
        if p.is_null() {
            return "";
        }
        // SAFETY: per the contract above.
        unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
    }

    /// Every callback runs on Qt's thread inside an `extern "C"` frame, so a
    /// panic must not unwind out of it.
    fn guarded<R>(default: R, f: impl FnOnce() -> R) -> R {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(default)
    }

    unsafe extern "C" fn cb_config_get(out: *mut Config) {
        guarded((), || {
            let (Some(b), false) = (backend(), out.is_null()) else {
                return;
            };
            let cfg = b.config();
            let mut c = Config {
                enabled: cfg.enabled,
                offset_ms: cfg.offset_ms,
                ntp_server: [0; 128],
            };
            copy_str(&mut c.ntp_server, &cfg.ntp_server);
            // SAFETY: the dock passes a live struct of exactly this type.
            unsafe { out.write(c) };
        });
    }

    unsafe extern "C" fn cb_set_enabled(enabled: bool) {
        guarded((), || {
            if let Some(b) = backend() {
                b.set_enabled(enabled);
            }
        });
    }

    unsafe extern "C" fn cb_set_offset_ms(offset_ms: i32) {
        guarded((), || {
            if let Some(b) = backend() {
                b.set_offset_ms(offset_ms);
            }
        });
    }

    unsafe extern "C" fn cb_set_ntp_server(host: *const c_char) {
        guarded((), || {
            if let Some(b) = backend() {
                // SAFETY: the dock hands a NUL-terminated string that lives
                // for the call.
                b.set_ntp_server(unsafe { arg_str(host) });
            }
        });
    }

    unsafe extern "C" fn cb_ntp_status(out: *mut NtpStatus) {
        guarded((), || {
            let (Some(b), false) = (backend(), out.is_null()) else {
                return;
            };
            let s = b.ntp_status();
            let mut c = NtpStatus {
                synced: s.synced,
                offset_ns: s.offset_ns,
                rtt_ns: s.rtt_ns,
                age_ns: s.age_ns,
                server: [0; 128],
                primary: [0; 128],
                burst_mode: s.burst_mode,
                fallback_active: s.fallback_active,
                drift_valid: s.drift_valid,
                drift_ppm: s.drift_ppm,
            };
            copy_str(&mut c.server, &s.server);
            copy_str(&mut c.primary, &s.primary);
            // SAFETY: the dock passes a live struct of exactly this type.
            unsafe { out.write(c) };
        });
    }

    unsafe extern "C" fn cb_utc_now_ns(out: *mut i64) -> bool {
        guarded(false, || {
            let (Some(b), false) = (backend(), out.is_null()) else {
                return false;
            };
            match b.utc_now_ns() {
                Some(utc) => {
                    // SAFETY: the dock passes a live i64.
                    unsafe { out.write(utc) };
                    true
                }
                None => false,
            }
        })
    }

    fn entry_to_c(e: &DockEntry) -> Entry {
        let mut out = Entry {
            source_name: [0; 256],
            sync_enabled: e.sync_enabled,
            snap: Snapshot {
                status: e.snap.status,
                tc_reason: e.snap.tc_reason,
                have_timecode: e.snap.timecode.is_some(),
                tc: match e.snap.timecode {
                    Some(tc) => Timecode {
                        hours: tc.hours,
                        minutes: tc.minutes,
                        seconds: tc.seconds,
                        n_frames: tc.n_frames,
                    },
                    None => Timecode {
                        hours: 0,
                        minutes: 0,
                        seconds: 0,
                        n_frames: 0,
                    },
                },
                latency_ms: e.snap.latency_ms,
                latency_peak_ms: e.snap.latency_peak_ms,
                added_ms: e.snap.added_ms,
                error_ms: e.snap.error_ms,
                required_offset_ms: e.snap.required_offset_ms,
            },
        };
        copy_str(&mut out.source_name, &e.source_name);
        out
    }

    unsafe extern "C" fn cb_collect(entries: *mut Entry, max: usize) -> usize {
        guarded(0, || {
            let (Some(b), false) = (backend(), entries.is_null()) else {
                return 0;
            };
            let all = b.collect();
            let n = all.len().min(max).min(MAX_SOURCES);
            for (i, e) in all.iter().take(n).enumerate() {
                // SAFETY: the dock passes an array of at least `max` entries.
                unsafe { entries.add(i).write(entry_to_c(e)) };
            }
            n
        })
    }

    unsafe extern "C" fn cb_log(level: c_int, message: *const c_char) {
        guarded((), || {
            if let Some(b) = backend() {
                let level = match level {
                    1 => LogLevel::Warning,
                    2 => LogLevel::Error,
                    _ => LogLevel::Info,
                };
                // SAFETY: the dock hands a NUL-terminated string that lives
                // for the call.
                b.log(level, unsafe { arg_str(message) });
            }
        });
    }

    /// The Rust view of the structs matches what the C++ was compiled with.
    fn abi_matches(backend: &dyn DockBackend) -> bool {
        let mut sizes = AbiSizes::default();
        // SAFETY: plain out-parameter write of a POD struct.
        unsafe { irl_sync_dock_abi_sizes(&raw mut sizes) };
        let expected = AbiSizes {
            timecode: size_of::<Timecode>(),
            snapshot: size_of::<Snapshot>(),
            entry: size_of::<Entry>(),
            config: size_of::<Config>(),
            ntp_status: size_of::<NtpStatus>(),
            api: size_of::<Api>(),
        };
        let same = sizes.timecode == expected.timecode
            && sizes.snapshot == expected.snapshot
            && sizes.entry == expected.entry
            && sizes.config == expected.config
            && sizes.ntp_status == expected.ntp_status
            && sizes.api == expected.api;
        if !same {
            backend.log(
                LogLevel::Error,
                "Sync dock ABI mismatch between the Rust and C++ halves; dock disabled",
            );
        }
        same
    }

    /// Register the dock with the OBS frontend, reading and writing through
    /// `backend` for the rest of the process. Returns whether the dock is
    /// registered; every failure is logged through the backend.
    pub fn register(backend: &'static dyn DockBackend) -> bool {
        if BACKEND.set(backend).is_err() {
            // Already registered once; the dock is a process-lifetime object.
            return true;
        }
        if !abi_matches(backend) {
            return false;
        }
        let limits = backend.limits();
        let default_server = DEFAULT_SERVER
            .get_or_init(|| CString::new(limits.default_ntp_server).unwrap_or_default());
        let api = Api {
            abi_version: ABI_VERSION,
            min_offset_ms: limits.min_offset_ms,
            max_offset_ms: limits.max_offset_ms,
            offset_step_ms: limits.offset_step_ms,
            default_ntp_server: default_server.as_ptr(),
            config_get: cb_config_get,
            set_enabled: cb_set_enabled,
            set_offset_ms: cb_set_offset_ms,
            set_ntp_server: cb_set_ntp_server,
            ntp_status: cb_ntp_status,
            utc_now_ns: cb_utc_now_ns,
            collect: cb_collect,
            log: Some(cb_log),
        };
        // SAFETY: `api` is a complete, correctly laid out table (checked
        // above); the C++ copies it during the call. Every function pointer
        // is a guarded `extern "C"` shim, and the backend and the default
        // server string live for the rest of the process.
        unsafe { irl_sync_dock_register(&raw const api) }
    }

    /// Remove the dock. Safe at shutdown as well as at a live unload: the
    /// frontend API returns without doing anything once the main window is
    /// gone.
    pub fn unregister() {
        // SAFETY: no arguments; idempotent on the C++ side.
        unsafe { irl_sync_dock_unregister() };
    }

    #[allow(dead_code)]
    fn _sizes_are_plain(_: *const c_void) {}
}
