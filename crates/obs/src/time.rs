//! The OBS clock. `obs_source_output_video`/`_audio` timestamps live in this
//! domain, so every timestamp the plugin derives must come from here, not from
//! `std::time::Instant` (different epoch) and not from FFmpeg's `av_gettime`
//! (microseconds, and used only for FFmpeg-side timers).

/// `os_gettime_ns()`.
pub fn gettime_ns() -> u64 {
    todo!("W1-A")
}

/// `os_sleep_ms()`.
pub fn sleep_ms(ms: u32) {
    let _ = ms;
    todo!("W1-A")
}
