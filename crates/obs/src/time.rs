//! The OBS clock. `obs_source_output_video`/`_audio` timestamps live in this
//! domain, so every timestamp the plugin derives must come from here, not from
//! `std::time::Instant` (different epoch) and not from FFmpeg's `av_gettime`
//! (microseconds, and used only for FFmpeg-side timers).

/// `os_gettime_ns()`.
#[must_use]
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

pub fn gettime_ns() -> u64 {
    // SAFETY: no arguments, no state; libobs's monotonic clock reader.
    unsafe { obs_sys::os_gettime_ns() }
}

/// `os_sleep_ms()`.
pub fn sleep_ms(ms: u32) {
    // SAFETY: plain value argument.
    unsafe { obs_sys::os_sleep_ms(ms) }
}
