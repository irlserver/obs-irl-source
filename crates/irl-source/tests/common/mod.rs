//! Fixtures shared by the integration tests: a configuration with the
//! shipped defaults, a `Shared` that needs no libobs, and a sink that records
//! what the audio pump submits.
//!
//! Each test binary compiles this module on its own and uses a different
//! subset of it, hence the `dead_code` allowance. Nothing here calls into
//! libobs, which keeps the binaries `scripts/test-with-libobs-shim.sh` links
//! against its stand-in down to the four functions the shim provides.

#![allow(dead_code)]

use std::ffi::CString;
use std::ptr::NonNull;
use std::sync::Arc;

use parking_lot::Mutex;

use irl_core::{HwDecode, Watermarks, consts};
use obs_irl_source::audio::AudioSink;
use obs_irl_source::config::Config;
use obs_irl_source::shared::{HotValues, LifetimeStats, Shared, StreamConfig, TimedPacket};

pub const RATE: i32 = 48_000;
pub const CHANNELS: i32 = 2;
/// One Opus frame at 48 kHz: 20 ms.
pub const CHUNK_FRAMES: i32 = 960;
pub const CHUNK_NS: u64 = 20_000_000;

pub fn stream_config(low_latency_audio: bool) -> StreamConfig {
    StreamConfig {
        url: CString::new("srt://example.invalid:9000").unwrap(),
        ffmpeg_options: None,
        hw_decode: HwDecode::Auto,
        low_latency_audio,
    }
}

/// The hot settings a freshly added source starts with, at `target_ms`.
pub fn hot_values(target_ms: i32) -> HotValues {
    HotValues {
        reconnect_delay_s: consts::DEFAULT_RECONNECT_DELAY_S as i32,
        adaptive_speed: consts::DEFAULT_ADAPTIVE_SPEED,
        catchup_percent: consts::DEFAULT_CATCHUP_PERCENT as i32,
        wait_for_keyframe: consts::DEFAULT_WAIT_FOR_KEYFRAME,
        clear_on_disconnect: consts::DEFAULT_CLEAR_ON_DISCONNECT,
        watermarks: Watermarks::derive(target_ms),
    }
}

/// A freshly added source's whole configuration, streaming from `url`.
pub fn default_config(url: &str) -> Config {
    Config {
        stream: StreamConfig {
            url: CString::new(url).unwrap(),
            ..stream_config(false)
        },
        hot: hot_values(consts::DEFAULT_BUFFER_TARGET_MS as i32),
        close_when_inactive: consts::DEFAULT_CLOSE_WHEN_INACTIVE,
    }
}

/// Runtime state for one connection, around a source handle that dangles.
/// Every path under test sends its output to a recording sink instead, so
/// nothing dereferences it.
pub fn shared(stream: StreamConfig, hot: HotValues) -> Arc<Shared> {
    // SAFETY: never dereferenced, see above.
    let source = unsafe { obs::SourceHandle::from_raw(NonNull::dangling()) };
    Shared::new(source, stream, hot, Arc::new(LifetimeStats::default()))
}

/// Queue a packet with no payload, stamped `pts_ns` and accounted as `bytes`.
pub fn push_packet(shared: &Shared, pts_ns: i64, bytes: usize) {
    shared.video.push_packet(
        TimedPacket {
            packet: ffmpeg::Packet::new().unwrap(),
            pts_ns,
            bytes,
            received_ns: 0,
        },
        &shared.lifetime,
    );
}

/// One audio submission, flattened.
pub struct Emitted {
    pub timestamp: u64,
    pub frames: u32,
    pub rate: u32,
    /// Interleaved PCM, or empty unless the recorder keeps samples.
    pub samples: Vec<f32>,
}

/// Audio sink that keeps every submission instead of handing it to libobs.
#[derive(Clone, Default)]
pub struct Recorder {
    pub emitted: Arc<Mutex<Vec<Emitted>>>,
    /// Off by default: the network simulations submit minutes of audio, and
    /// copying all of it would cost hundreds of megabytes per run for tests
    /// that only read the clock line.
    keep_samples: bool,
}

impl Recorder {
    /// A recorder that also copies each submission's PCM.
    pub fn with_samples() -> Self {
        Self {
            keep_samples: true,
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.emitted.lock().len()
    }
}

impl AudioSink for Recorder {
    fn output_audio(&self, audio: &obs::AudioFrame<'_>) {
        let sys = audio.as_sys();
        let samples = if self.keep_samples {
            let bytes = sys.frames as usize * CHANNELS as usize * 4;
            // SAFETY: the frame borrows a live interleaved-float buffer of
            // `frames * CHANNELS` samples, which is what the pump built.
            let raw = unsafe { std::slice::from_raw_parts(sys.data[0], bytes) };
            raw.chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        } else {
            Vec::new()
        };
        self.emitted.lock().push(Emitted {
            timestamp: sys.timestamp,
            frames: sys.frames,
            rate: sys.samples_per_sec,
            samples,
        });
    }
}
