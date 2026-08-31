//! Hardware-decode probe order, the forced-NVDEC pixel format decision and the
//! reconnect countdown (`src/receiver/probe.rs`).
//!
//! The module is included by path rather than imported: `crate::receiver` is
//! private to the plugin crate and an integration test is a separate crate.
//! `probe.rs` depends on nothing but the `ffmpeg` crate, so the copy behaves
//! identically to the one the plugin builds.

#[path = "../src/receiver/probe.rs"]
mod probe;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use ffmpeg::{AVCodecID, AVHWDeviceType, AVPixelFormat, Codec};

#[test]
fn hardware_probe_order_matches_the_platform() {
    #[cfg(windows)]
    assert_eq!(
        probe::HW_DEVICE_TYPES,
        &[
            AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
        ]
    );

    #[cfg(target_os = "macos")]
    assert_eq!(
        probe::HW_DEVICE_TYPES,
        &[AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX]
    );

    #[cfg(not(any(windows, target_os = "macos")))]
    assert_eq!(
        probe::HW_DEVICE_TYPES,
        &[
            AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
        ]
    );

    // The platform-native API always leads; CUDA is the fallback where it is
    // on the list at all, and never appears twice.
    assert!(!probe::HW_DEVICE_TYPES.is_empty());
    assert_eq!(
        probe::HW_DEVICE_TYPES
            .iter()
            .filter(|kind| **kind == AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA)
            .count(),
        usize::from(probe::HW_DEVICE_TYPES.len() > 1)
    );
}

#[test]
fn forced_nvdec_probes_cuda_and_nothing_else() {
    assert_eq!(
        probe::NVDEC_DEVICE_TYPES,
        &[AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA]
    );
}

/// Every CUDA pixel format the real H.264 decoder declares through the
/// `hw_device_ctx` method — the set `pick_cuda_format` is allowed to return.
fn cuda_formats(codec: &Codec) -> Vec<AVPixelFormat> {
    codec
        .hw_configs()
        .into_iter()
        .filter(|config| {
            config.device_type == AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA
                && (config.methods & (ffmpeg::sys::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32))
                    != 0
        })
        .map(|config| config.pix_fmt)
        .collect()
}

#[test]
fn nvdec_format_selection_skips_software_formats() {
    let codec = Codec::find_decoder(AVCodecID::AV_CODEC_ID_H264).expect("h264 decoder");

    // Never a software format, whatever the build offers.
    assert_eq!(
        probe::pick_cuda_format(&codec, &[AVPixelFormat::AV_PIX_FMT_YUV420P]),
        AVPixelFormat::AV_PIX_FMT_NONE
    );
    assert_eq!(
        probe::pick_cuda_format(&codec, &[]),
        AVPixelFormat::AV_PIX_FMT_NONE
    );

    match cuda_formats(&codec).first().copied() {
        Some(cuda) => {
            // The CUDA format wins even when a software format is offered
            // first: the C loop returns the first *offered* format that any
            // CUDA config declares.
            assert_eq!(
                probe::pick_cuda_format(&codec, &[AVPixelFormat::AV_PIX_FMT_YUV420P, cuda]),
                cuda
            );
            assert_eq!(probe::pick_cuda_format(&codec, &[cuda]), cuda);
        }
        None => {
            // No CUDA support compiled in: forced NVDEC must fail the open
            // rather than silently decode in software.
            assert_eq!(
                probe::pick_cuda_format(&codec, &[AVPixelFormat::AV_PIX_FMT_CUDA]),
                AVPixelFormat::AV_PIX_FMT_NONE
            );
        }
    }
}

#[test]
fn reconnect_wait_returns_at_once_when_the_run_is_already_stopped() {
    let active = AtomicBool::new(false);
    let start = Instant::now();
    probe::reconnect_sleep(60, &active);
    assert!(start.elapsed() < Duration::from_millis(500));
}

#[test]
fn reconnect_wait_abandons_the_countdown_when_thread_active_goes_false() {
    let active = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&active);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        flag.store(false, Relaxed);
    });

    let start = Instant::now();
    probe::reconnect_sleep(60, &active);
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "reconnect wait ignored the stop request ({elapsed:?})"
    );
}

#[test]
fn a_non_positive_reconnect_delay_never_sleeps() {
    let active = AtomicBool::new(true);
    for delay_s in [-1, 0] {
        let start = Instant::now();
        probe::reconnect_sleep(delay_s, &active);
        assert!(start.elapsed() < Duration::from_millis(500));
    }
}
