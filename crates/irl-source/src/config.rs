//! Settings loading, restart diffing and hot apply (port of
//! `config_load` / `config_requires_restart` / `config_apply_hot`).

use std::ffi::{CStr, CString};
use std::sync::atomic::Ordering::Relaxed;

use irl_core::{HwDecode, Watermarks, consts};
use obs::Data;

use crate::shared::{HotValues, Shared, StreamConfig};

/// NVDEC is only offered (and only compiled into the bundled FFmpeg) on
/// Windows and Linux. A scene collection saved there can still carry the
/// value to a Mac; `HwDecode::from_i64` degrades it to Auto rather than
/// forcing a CUDA device that cannot exist, which would leave the source
/// videoless.
pub const NVDEC_AVAILABLE: bool = cfg!(any(windows, target_os = "linux"));

/// The authoritative configuration held by the OBS thread.
#[derive(Debug, Clone)]
pub struct Config {
    pub stream: StreamConfig,
    pub hot: HotValues,
    pub close_when_inactive: bool,
}

impl Config {
    /// `config_load`.
    pub fn load(settings: &Data<'_>) -> Self {
        // `Data::get_str` already returns `None` for an empty string, which
        // is the C `url && *url` idiom. An interior NUL cannot survive
        // `obs_data_get_string`, but a lossy conversion is still cheaper than
        // a panic path: an unusable URL becomes "no URL".
        let url = settings
            .get_str(c"url")
            .and_then(|s| CString::new(s).ok())
            .unwrap_or_default();

        let hw_raw = settings.get_i64(c"hw_decode");
        let (hw_decode, degraded) = HwDecode::from_i64(hw_raw, NVDEC_AVAILABLE);
        if degraded {
            if hw_raw == HwDecode::Nvdec.as_i64() {
                irl_warn!("NVDEC is not available on this platform; using Auto");
            } else {
                irl_warn!("Unknown hardware decode mode {hw_raw}; using Auto");
            }
        }

        Self {
            stream: StreamConfig {
                url,
                ffmpeg_options: settings.get_str(c"ffmpeg_options"),
                hw_decode,
                low_latency_audio: settings.get_bool(c"low_latency_audio"),
                small_gap_ms: consts::SMALL_GAP_MS,
                large_gap_ms: consts::LARGE_GAP_MS,
            },
            hot: HotValues {
                reconnect_delay_s: settings.get_i64(c"reconnect_delay") as i32,
                adaptive_speed: settings.get_bool(c"adaptive_speed"),
                // The slider bounds this, but a scene collection can carry
                // anything, including a value saved by a build with different
                // bounds.
                catchup_percent: (settings.get_i64(c"catchup_percent") as i32)
                    .clamp(consts::CATCHUP_PERCENT_MIN, consts::CATCHUP_PERCENT_MAX),
                wait_for_keyframe: settings.get_bool(c"wait_for_keyframe"),
                clear_on_disconnect: settings.get_bool(c"clear_on_disconnect"),
                // A non-positive target falls back to the default, as
                // `config_load` does.
                watermarks: Watermarks::derive(settings.get_i64(c"buffer_target_ms") as i32),
            },
            close_when_inactive: settings.get_bool(c"close_when_inactive"),
        }
    }

    /// The configured URL, or `None` when the source has none yet.
    pub fn url(&self) -> Option<&CStr> {
        (!self.stream.url.is_empty()).then_some(self.stream.url.as_c_str())
    }

    /// Which settings force a reconnect. `url` and `ffmpeg_options` are
    /// consumed by `avformat_open_input`, `hw_decode` picks the decoder at
    /// open, and `low_latency_audio` latches priming/pump semantics across
    /// all three threads. Everything else is re-read live every cycle, so it
    /// can be swapped in place: a restart costs an SRT handshake and wipes
    /// every per-connection stat counter.
    pub fn requires_restart(&self, other: &Self) -> bool {
        self.stream.url != other.stream.url
            || self.stream.ffmpeg_options != other.stream.ffmpeg_options
            || self.stream.hw_decode != other.stream.hw_decode
            || self.stream.low_latency_audio != other.stream.low_latency_audio
    }

    /// `config_apply_hot`: swap the live settings into a running receiver.
    ///
    /// Lock order is the documented one: `audio_state`, then the jitter
    /// buffer, then the watermark mutex. Nothing below takes any of them.
    ///
    /// The ring grows before the new watermarks are published, and they are
    /// published only if that succeeded: the receiver's backpressure ceiling
    /// is 3x `max_ms` and must never exceed ring capacity, or a burst between
    /// fill checks would push writes past the end and drop audio. On failure
    /// the old target stays in force — including in the caller's copy of the
    /// config, which is why the effective watermarks are returned.
    pub fn apply_hot(&self, shared: &Shared) -> Watermarks {
        let _state = shared.audio_state();

        let current = shared.hot.watermarks();
        let next = self.hot.watermarks;
        let mut effective = current;

        if current.target_ms != next.target_ms {
            let resized = match shared.audio_buf().as_mut() {
                Some(buf) => buf.resize(next.target_ms, next.min_ms, next.max_ms),
                // No audio frame has configured the ring yet; the first one
                // sizes it from the watermarks published below.
                None => true,
            };
            if resized {
                *shared.hot.watermarks.lock() = next;
                effective = next;
            } else {
                irl_warn!(
                    "Could not resize jitter buffer to {}ms; keeping {}ms",
                    next.target_ms,
                    current.target_ms
                );
            }
        }

        shared
            .hot
            .reconnect_delay_s
            .store(self.hot.reconnect_delay_s, Relaxed);
        shared
            .hot
            .adaptive_speed
            .store(self.hot.adaptive_speed, Relaxed);
        shared
            .hot
            .catchup_percent
            .store(self.hot.catchup_percent, Relaxed);
        shared
            .hot
            .wait_for_keyframe
            .store(self.hot.wait_for_keyframe, Relaxed);
        shared
            .hot
            .clear_on_disconnect
            .store(self.hot.clear_on_disconnect, Relaxed);

        effective
    }
}
