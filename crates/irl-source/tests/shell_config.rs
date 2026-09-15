//! Settings diffing and hardware-decode degradation.
//!
//! No libobs runs under `cargo test`, so nothing here calls into it: the
//! configuration is built directly rather than loaded from an `obs_data_t`.

use std::ffi::CString;

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use irl_core::{HwDecode, Watermarks, consts};
use obs_irl_source::config::Config;
use obs_irl_source::shared::{HotValues, LifetimeStats, Shared, StreamConfig};

fn config() -> Config {
    Config {
        stream: StreamConfig {
            url: CString::new("srt://example.invalid:9000").unwrap(),
            ffmpeg_options: None,
            hw_decode: HwDecode::Auto,
            low_latency_audio: false,
            compensate_av_skew: consts::DEFAULT_COMPENSATE_AV_SKEW,
            small_gap_ms: consts::SMALL_GAP_MS,
            large_gap_ms: consts::LARGE_GAP_MS,
        },
        hot: HotValues {
            reconnect_delay_s: consts::DEFAULT_RECONNECT_DELAY_S as i32,
            adaptive_speed: consts::DEFAULT_ADAPTIVE_SPEED,
            catchup_percent: consts::DEFAULT_CATCHUP_PERCENT as i32,
            wait_for_keyframe: consts::DEFAULT_WAIT_FOR_KEYFRAME,
            clear_on_disconnect: consts::DEFAULT_CLEAR_ON_DISCONNECT,
            watermarks: Watermarks::derive(consts::DEFAULT_BUFFER_TARGET_MS as i32),
        },
        close_when_inactive: consts::DEFAULT_CLOSE_WHEN_INACTIVE,
    }
}

#[test]
fn an_unchanged_config_does_not_restart() {
    assert!(!config().requires_restart(&config()));
}

#[test]
fn the_five_latched_settings_force_a_restart() {
    let base = config();

    let mut url = config();
    url.stream.url = CString::new("srt://elsewhere.invalid:9000").unwrap();
    assert!(base.requires_restart(&url));

    let mut options = config();
    options.stream.ffmpeg_options = Some("latency=500000".into());
    assert!(base.requires_restart(&options));

    let mut hw = config();
    hw.stream.hw_decode = HwDecode::Off;
    assert!(base.requires_restart(&hw));

    let mut low_latency = config();
    low_latency.stream.low_latency_audio = true;
    assert!(base.requires_restart(&low_latency));

    let mut lip_sync = config();
    lip_sync.stream.compensate_av_skew = !consts::DEFAULT_COMPENSATE_AV_SKEW;
    assert!(base.requires_restart(&lip_sync));
}

/// The published target is the user's Target Buffer plus the connection's
/// audio hold. A Target Buffer edit mid-stream must keep the hold, and the
/// config the OBS thread diffs against next time must carry the user's
/// value, not the composed one.
#[test]
fn a_hot_target_edit_keeps_the_audio_hold() {
    // SAFETY: nothing here dereferences the handle; no libobs is running and
    // `apply_hot` only touches the plugin's own state.
    let source = unsafe { obs::SourceHandle::from_raw(NonNull::dangling()) };
    let base = config();
    let shared = Shared::new(
        source,
        base.stream.clone(),
        base.hot,
        Arc::new(LifetimeStats::default()),
    );

    // A connection measured a 1550 ms hold on a 120 ms target.
    shared.conn.av_skew_hold_ms.store(1550, Relaxed);
    *shared.hot.watermarks.lock() = Watermarks::derive(120 + 1550);

    let mut edited = config();
    edited.hot.watermarks = Watermarks::derive(500);
    let effective = edited.apply_hot(&shared);

    assert_eq!(
        effective.target_ms, 500,
        "the user's value is what is diffed next time"
    );
    assert_eq!(
        shared.hot.watermarks().target_ms,
        500 + 1550,
        "the hold rides on the new target"
    );
    assert_eq!(shared.conn.av_skew_hold_ms.load(Relaxed), 1550);
}

#[test]
fn every_other_setting_is_applied_in_place() {
    let base = config();

    let mut delay = config();
    delay.hot.reconnect_delay_s = 30;
    assert!(!base.requires_restart(&delay));

    let mut buffer = config();
    buffer.hot.watermarks = Watermarks::derive(2000);
    assert!(!base.requires_restart(&buffer));

    let mut adaptive = config();
    adaptive.hot.adaptive_speed = !base.hot.adaptive_speed;
    assert!(!base.requires_restart(&adaptive));

    let mut catchup = config();
    catchup.hot.catchup_percent = consts::CATCHUP_PERCENT_MAX;
    assert!(!base.requires_restart(&catchup));

    let mut keyframe = config();
    keyframe.hot.wait_for_keyframe = !base.hot.wait_for_keyframe;
    assert!(!base.requires_restart(&keyframe));

    let mut clear = config();
    clear.hot.clear_on_disconnect = !base.hot.clear_on_disconnect;
    assert!(!base.requires_restart(&clear));

    let mut inactive = config();
    inactive.close_when_inactive = !base.close_when_inactive;
    assert!(!base.requires_restart(&inactive));
}

#[test]
fn an_empty_url_reads_as_no_url() {
    let mut cfg = config();
    assert!(cfg.url().is_some());
    cfg.stream.url = CString::default();
    assert!(cfg.url().is_none());
}

/// The setting is only offered where the bundled FFmpeg has CUDA, but a scene
/// collection saved on Windows or Linux can carry it to a Mac.
#[test]
fn nvdec_survives_only_where_it_exists() {
    let (mode, degraded) = HwDecode::from_i64(
        HwDecode::Nvdec.as_i64(),
        obs_irl_source::config::NVDEC_AVAILABLE,
    );
    if cfg!(any(windows, target_os = "linux")) {
        assert_eq!((mode, degraded), (HwDecode::Nvdec, false));
    } else {
        assert_eq!((mode, degraded), (HwDecode::Auto, true));
    }
}
