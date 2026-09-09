//! The OBS clock. `obs_source_output_video`/`_audio` timestamps live in this
//! domain, so every timestamp the plugin derives must come from here, not from
//! `std::time::Instant` (different epoch) and not from FFmpeg's `av_gettime`
//! (microseconds, and used only for FFmpeg-side timers).

/// `obs_get_frame_interval_ns`: nanoseconds between canvas render ticks, or
/// `None` before video is set up.
///
/// The canvas frame rate is a setting the user can change while a source runs,
/// so callers that pace against it should sample rather than cache.
pub fn canvas_frame_interval_ns() -> Option<u64> {
    // SAFETY: no arguments; libobs returns 0 when video is not initialised.
    let ns = unsafe { obs_sys::obs_get_frame_interval_ns() };
    (ns != 0).then_some(ns)
}

/// `os_gettime_ns()`.
#[must_use]
pub fn gettime_ns() -> u64 {
    // SAFETY: no arguments, no state; libobs's monotonic clock reader.
    unsafe { obs_sys::os_gettime_ns() }
}

/// Block the calling thread for at least `ms` milliseconds.
///
/// Deliberately `std::thread::sleep` and not libobs's `os_sleep_ms`. On
/// Windows 8+ that function subtracts one millisecond before calling `Sleep`
/// to compensate for the scheduler's coarse timer, so `os_sleep_ms(1)` is
/// `Sleep(0)`: a yield, not a wait. The audio pump's minimum wait is 1 ms,
/// which turned every idle source into a busy loop pinning a core, and the
/// last millisecond before an audio deadline into a spin. `std::thread::sleep`
/// never returns early, and on Windows 10 1803+ it uses a high-resolution
/// waitable timer, so 1 ms means about 1 ms rather than a 15.6 ms quantum.
///
/// Only the waiting moves off libobs; timestamps still come from
/// [`gettime_ns`].
pub fn sleep_ms(ms: u32) {
    std::thread::sleep(std::time::Duration::from_millis(u64::from(ms)));
}
