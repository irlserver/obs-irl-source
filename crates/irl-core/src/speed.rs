//! Playback-speed controller and stuck-drain watch (ports of
//! `compute_buffered_output_speed` and `audio_check_drain_progress`).

use crate::config::Watermarks;

/// Asymmetric speed controller: builds at up to −2 %, drains at up to +5 %,
/// with a ±20 ms deadband around target and an EMA of 0.05 per step.
#[derive(Debug, Clone)]
pub struct SpeedController {
    current: f32,
}

impl SpeedController {
    /// Starts at 1.0.
    pub fn new() -> Self {
        Self { current: 1.0 }
    }

    /// Advance one pump cycle and return the smoothed speed.
    pub fn update(&mut self, fill_ms: i32, wm: Watermarks, adaptive: bool, low_latency: bool) -> f32 {
        let _ = (fill_ms, wm, adaptive, low_latency);
        todo!("W1-C")
    }

    /// The smoothed speed.
    pub fn current(&self) -> f32 {
        self.current
    }

    /// Reset to 1.0 (reconnect).
    pub fn reset(&mut self) {
        self.current = 1.0;
    }
}

impl Default for SpeedController {
    fn default() -> Self {
        Self::new()
    }
}

/// A drain that has run at full authority without progress for 20 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StuckReport {
    /// Fill when the watch started.
    pub start_fill_ms: i32,
    /// Fill now.
    pub fill_ms: i32,
    /// Seconds stuck.
    pub stuck_s: u64,
}

/// State of the stuck-drain detector.
#[derive(Debug, Clone, Default)]
pub struct DrainWatch {
    since_us: u64,
    fill_ms: i32,
    warn_time_us: u64,
}

impl DrainWatch {
    /// Observe one cycle. Returns a report when the warning should be logged
    /// (at most once per 20 s).
    pub fn observe(&mut self, fill_ms: i32, speed: f32, target_ms: i32, now_us: u64) -> Option<StuckReport> {
        let _ = (fill_ms, speed, target_ms, now_us, self.since_us, self.fill_ms, self.warn_time_us);
        todo!("W1-C")
    }
}
