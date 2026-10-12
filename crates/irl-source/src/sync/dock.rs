//! The IRL Sync dock's view of the plugin: the backend the `sync-dock` crate's
//! Qt widget reads settings and status through, and writes the three global
//! settings back through.
//!
//! The dock itself is the C branch's `sync-dock.cpp`, compiled into the
//! `sync-dock` crate when Qt6 is found at build time. Without Qt that crate is
//! a stub and this registers nothing; sync still runs headless and its status
//! stays readable over the obs-websocket vendor (`GetSyncStatus`), which can
//! also set the three settings (`SetSyncConfig`).

use irl_core::consts;
use irl_core::sync::{SyncStatus, TcReason};
use sync_dock::{
    DockBackend, DockConfig, DockEntry, DockLimits, DockNtpStatus, DockSnapshot, DockTimecode,
    LogLevel,
};

struct Backend;

static BACKEND: Backend = Backend;

fn status_code(status: SyncStatus) -> i32 {
    match status {
        SyncStatus::Off => sync_dock::STATUS_OFF,
        SyncStatus::NoTimecode => sync_dock::STATUS_NO_TIMECODE,
        SyncStatus::Stale => sync_dock::STATUS_STALE,
        SyncStatus::TooSlow => sync_dock::STATUS_TOO_SLOW,
        SyncStatus::Acquiring => sync_dock::STATUS_ACQUIRING,
        SyncStatus::Locked => sync_dock::STATUS_LOCKED,
    }
}

fn reason_code(reason: TcReason) -> i32 {
    match reason {
        TcReason::Ok => sync_dock::REASON_OK,
        TcReason::NoClock => sync_dock::REASON_NO_CLOCK,
        TcReason::Codec => sync_dock::REASON_CODEC,
        TcReason::Absent => sync_dock::REASON_ABSENT,
    }
}

impl DockBackend for Backend {
    fn limits(&self) -> DockLimits {
        DockLimits {
            min_offset_ms: consts::SYNC_MIN_OFFSET_MS,
            max_offset_ms: consts::SYNC_MAX_OFFSET_MS,
            offset_step_ms: consts::SYNC_OFFSET_STEP_MS,
            default_ntp_server: consts::SYNC_DEFAULT_NTP_SERVER,
        }
    }

    fn config(&self) -> DockConfig {
        let cfg = super::config();
        DockConfig {
            enabled: cfg.enabled,
            offset_ms: cfg.offset_ms,
            ntp_server: cfg.ntp_server,
        }
    }

    fn set_enabled(&self, enabled: bool) {
        super::set_enabled(enabled);
    }

    fn set_offset_ms(&self, offset_ms: i32) {
        super::set_offset_ms(offset_ms);
    }

    fn set_ntp_server(&self, host: &str) {
        super::set_ntp_server(host);
    }

    fn ntp_status(&self) -> DockNtpStatus {
        let s = super::ntp::status();
        DockNtpStatus {
            synced: s.synced,
            offset_ns: s.offset_ns,
            rtt_ns: s.rtt_ns,
            age_ns: s.age_ns,
            server: s.server,
            primary: s.primary,
            burst_mode: s.burst_mode,
            fallback_active: s.fallback_active,
            drift_valid: s.drift_valid,
            drift_ppm: s.drift_ppm,
        }
    }

    fn utc_now_ns(&self) -> Option<i64> {
        super::ntp::utc_now_ns()
    }

    fn collect(&self) -> Vec<DockEntry> {
        super::collect()
            .into_iter()
            .map(|entry| DockEntry {
                source_name: entry.source_name,
                sync_enabled: entry.sync_enabled,
                snap: DockSnapshot {
                    status: status_code(entry.snap.status),
                    tc_reason: reason_code(entry.snap.tc_reason),
                    timecode: entry.snap.timecode.map(|tc| DockTimecode {
                        hours: tc.hours,
                        minutes: tc.minutes,
                        seconds: tc.seconds,
                        n_frames: tc.n_frames,
                    }),
                    latency_ms: entry.snap.latency_ms,
                    latency_peak_ms: entry.snap.latency_peak_ms,
                    added_ms: entry.snap.added_ms,
                    error_ms: entry.snap.error_ms,
                    required_offset_ms: entry.snap.required_offset_ms,
                },
            })
            .collect()
    }

    fn log(&self, level: LogLevel, message: &str) {
        match level {
            LogLevel::Info => irl_info!("{message}"),
            LogLevel::Warning => irl_warn!("{message}"),
            LogLevel::Error => irl_error!("{message}"),
        }
    }
}

/// `irl_sync_dock_register`, from `obs_module_post_load`.
pub fn register() {
    if !sync_dock::available() {
        irl_info!(
            "Built without the IRL Sync dock (no Qt6 at build time); sync status is still on the websocket vendor"
        );
        return;
    }
    sync_dock::register(&BACKEND);
}

/// `irl_sync_dock_unregister`, from `obs_module_unload`.
pub fn unregister() {
    sync_dock::unregister();
}
