//! Hardware-decode probe tables and the two receiver helpers that are pure
//! enough to test on their own (port of the tables and `nvdec_get_format`
//! in `src/receiver-stream.c`).
//!
//! Nothing here touches plugin state, so `crates/irl-source/tests/receiver_*.rs`
//! includes this file directly with `#[path]` — `crate::receiver` is private,
//! and an integration test is a separate crate.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use ffmpeg::{AVHWDeviceType, AVPixelFormat, Codec};

/// Device types tried, in order, when hardware decode is on "Auto".
///
/// Windows D3D11VA → CUDA, macOS VideoToolbox, everything else VAAPI → CUDA.
/// The first device that is created wins; `HwDeviceContext::probe` reports
/// each failure so the caller can log it the way the C plugin does.
#[cfg(windows)]
pub const HW_DEVICE_TYPES: &[AVHWDeviceType] = &[
    AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
    AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
];

/// See the Windows table above.
#[cfg(target_os = "macos")]
pub const HW_DEVICE_TYPES: &[AVHWDeviceType] = &[AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX];

/// See the Windows table above.
#[cfg(not(any(windows, target_os = "macos")))]
pub const HW_DEVICE_TYPES: &[AVHWDeviceType] = &[
    AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
    AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
];

/// Explicit NVDEC probes CUDA and nothing else.
pub const NVDEC_DEVICE_TYPES: &[AVHWDeviceType] = &[AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA];

/// The decision inside `nvdec_get_format`: the first offered format that a
/// `AV_HWDEVICE_TYPE_CUDA` hardware config with `HW_DEVICE_CTX` support
/// declares, or `AV_PIX_FMT_NONE`.
///
/// Explicit NVDEC must not let libavcodec choose the software pixel format
/// when a stream or driver cannot provide CUDA frames. Auto deliberately keeps
/// the default FFmpeg negotiation, including its existing software fallback.
pub fn pick_cuda_format(codec: &Codec, offered: &[AVPixelFormat]) -> AVPixelFormat {
    let configs = codec.hw_configs();
    for &fmt in offered {
        for config in &configs {
            if config.device_type == AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA
                && (config.methods & (ffmpeg::sys::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32))
                    != 0
                && config.pix_fmt == fmt
            {
                return fmt;
            }
        }
    }
    AVPixelFormat::AV_PIX_FMT_NONE
}

/// The reconnect countdown: `delay_s` seconds in 100 ms steps, abandoned as
/// soon as the run is stopped so a stop request never waits out a 60 s delay.
pub fn reconnect_sleep(delay_s: i32, active: &AtomicBool) {
    let steps = delay_s.saturating_mul(10);
    let mut step = 0;
    while step < steps && active.load(Relaxed) {
        ffmpeg::usleep(100_000);
        step += 1;
    }
}
