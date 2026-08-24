//! Playback-speed controller and stuck-drain watch (ports of
//! `compute_buffered_output_speed` and `audio_check_drain_progress`).

use crate::config::Watermarks;
use crate::consts;

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
    ///
    /// Adaptive latency control off, or low-latency audio on, pins the speed
    /// at 1.0 immediately: there is no buffer to regulate.
    pub fn update(
        &mut self,
        fill_ms: i32,
        wm: Watermarks,
        adaptive: bool,
        low_latency: bool,
    ) -> f32 {
        if !adaptive || low_latency {
            self.current = 1.0;
            return 1.0;
        }

        let low_edge = wm.target_ms - consts::AUDIO_SPEED_DEADBAND_MS;
        let high_edge = wm.target_ms + consts::AUDIO_SPEED_DEADBAND_MS;
        let mut target_speed = 1.0f32;

        if fill_ms < low_edge {
            let span = low_edge - wm.min_ms;
            let mut t = if span > 0 {
                (low_edge - fill_ms) as f32 / span as f32
            } else {
                1.0
            };
            if t > 1.0 {
                t = 1.0;
            }
            target_speed = 1.0 - (1.0 - consts::AUDIO_SPEED_MIN) * t;
        } else if fill_ms > high_edge {
            let span = wm.max_ms - high_edge;
            let mut t = if span > 0 {
                (fill_ms - high_edge) as f32 / span as f32
            } else {
                1.0
            };
            if t > 1.0 {
                t = 1.0;
            }
            target_speed = 1.0 + (consts::AUDIO_SPEED_MAX - 1.0) * t;
        }

        if self.current <= 0.0 {
            self.current = 1.0;
        }
        self.current += (target_speed - self.current) * consts::AUDIO_SPEED_SMOOTHING;
        self.current = self
            .current
            .clamp(consts::AUDIO_SPEED_MIN, consts::AUDIO_SPEED_MAX);
        self.current
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
///
/// The drain is bounded at +5 %, so a sender whose media clock runs faster
/// than that can never be caught up with: the buffer rises to the read loop's
/// bleed ceiling and parks there. Nothing the plugin may do fixes that, but it
/// should not look like normal operation either.
#[derive(Debug, Clone, Default)]
pub struct DrainWatch {
    since_us: u64,
    fill_ms: i32,
    warn_time_us: u64,
}

impl DrainWatch {
    /// Observe one cycle. Returns a report when the warning should be logged
    /// (at most once per 20 s).
    pub fn observe(
        &mut self,
        fill_ms: i32,
        speed: f32,
        target_ms: i32,
        now_us: u64,
    ) -> Option<StuckReport> {
        let at_full_authority = speed >= consts::AUDIO_SPEED_MAX - 0.0005
            && fill_ms > target_ms + consts::AUDIO_SPEED_DEADBAND_MS;

        if !at_full_authority {
            self.since_us = 0;
            return None;
        }

        if self.since_us == 0 {
            self.since_us = now_us;
            self.fill_ms = fill_ms;
            return None;
        }

        // Coming down, just slowly: not stuck.
        if fill_ms <= self.fill_ms - consts::AUDIO_DRAIN_STUCK_PROGRESS_MS {
            self.since_us = now_us;
            self.fill_ms = fill_ms;
            return None;
        }

        if now_us - self.since_us < consts::AUDIO_DRAIN_STUCK_US {
            return None;
        }
        if self.warn_time_us != 0 && now_us - self.warn_time_us < consts::AUDIO_DRAIN_STUCK_US {
            return None;
        }
        self.warn_time_us = now_us;

        Some(StuckReport {
            start_fill_ms: self.fill_ms,
            fill_ms,
            stuck_s: (now_us - self.since_us) / 1_000_000,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WM: Watermarks = Watermarks {
        target_ms: 120,
        min_ms: 60,
        max_ms: 320,
    };

    /// Run the controller until it settles, so a test can look at the target
    /// the EMA is converging on rather than one 5 % step towards it.
    fn settle(fill_ms: i32) -> f32 {
        let mut c = SpeedController::new();
        let mut speed = 1.0;
        for _ in 0..2000 {
            speed = c.update(fill_ms, WM, true, false);
        }
        speed
    }

    #[test]
    fn deadband_holds_speed_at_one() {
        assert_eq!(settle(120), 1.0);
        assert_eq!(settle(110), 1.0);
        assert_eq!(settle(130), 1.0);
    }

    #[test]
    fn deadband_edges_are_inclusive() {
        // fill < target-20 and fill > target+20 are the only active regions,
        // so both edges themselves are still 1.0.
        assert_eq!(settle(100), 1.0);
        assert_eq!(settle(140), 1.0);
        assert!(settle(99) < 1.0);
        assert!(settle(141) > 1.0);
    }

    #[test]
    fn full_authority_at_the_watermarks() {
        // At (and below) buffer_min the build runs at the -2 % floor.
        assert!((settle(WM.min_ms) - consts::AUDIO_SPEED_MIN).abs() < 1e-4);
        assert!((settle(0) - consts::AUDIO_SPEED_MIN).abs() < 1e-4);
        // At (and above) buffer_max the drain runs at the +5 % ceiling.
        assert!((settle(WM.max_ms) - consts::AUDIO_SPEED_MAX).abs() < 1e-4);
        assert!((settle(5000) - consts::AUDIO_SPEED_MAX).abs() < 1e-4);
    }

    #[test]
    fn speed_is_clamped_to_the_authority_band() {
        for fill in [-1000, 0, 60, 120, 320, 100_000] {
            let s = settle(fill);
            assert!(
                (consts::AUDIO_SPEED_MIN..=consts::AUDIO_SPEED_MAX).contains(&s),
                "fill {fill} gave {s}"
            );
        }
    }

    #[test]
    fn ema_moves_five_percent_of_the_way_per_step() {
        let mut c = SpeedController::new();
        // A full-authority drain target of 1.05 from 1.0: the first step is
        // 5 % of the 0.05 distance.
        let first = c.update(WM.max_ms, WM, true, false);
        assert!((first - (1.0 + 0.05 * 0.05)).abs() < 1e-6, "got {first}");
        let second = c.update(WM.max_ms, WM, true, false);
        assert!((second - (first + (1.05 - first) * 0.05)).abs() < 1e-6);
    }

    #[test]
    fn ramp_is_linear_between_deadband_and_watermark() {
        // Halfway between the high edge (140) and buffer_max (320) is +2.5 %.
        let mid = settle(140 + (320 - 140) / 2);
        assert!((mid - 1.025).abs() < 1e-3, "got {mid}");
    }

    #[test]
    fn adaptive_off_pins_speed_to_one() {
        let mut c = SpeedController::new();
        c.update(WM.max_ms, WM, true, false);
        assert!(c.current() > 1.0);
        assert_eq!(c.update(WM.max_ms, WM, false, false), 1.0);
        assert_eq!(c.current(), 1.0);
    }

    #[test]
    fn low_latency_pins_speed_to_one() {
        let mut c = SpeedController::new();
        c.update(0, WM, true, false);
        assert!(c.current() < 1.0);
        assert_eq!(c.update(0, WM, true, true), 1.0);
        assert_eq!(c.current(), 1.0);
    }

    #[test]
    fn reset_returns_to_one() {
        let mut c = SpeedController::new();
        c.update(WM.max_ms, WM, true, false);
        c.reset();
        assert_eq!(c.current(), 1.0);
    }

    #[test]
    fn degenerate_watermarks_go_straight_to_full_authority() {
        // min above the low edge / max below the high edge: span <= 0, so t
        // is pinned at 1.0 rather than dividing by zero.
        let wm = Watermarks {
            target_ms: 120,
            min_ms: 200,
            max_ms: 100,
        };
        let mut c = SpeedController::new();
        let mut low = 1.0;
        let mut high = 1.0;
        for _ in 0..2000 {
            low = c.update(0, wm, true, false);
        }
        assert!((low - consts::AUDIO_SPEED_MIN).abs() < 1e-4);
        c.reset();
        for _ in 0..2000 {
            high = c.update(1000, wm, true, false);
        }
        assert!((high - consts::AUDIO_SPEED_MAX).abs() < 1e-4);
    }

    // ── DrainWatch ──

    const SEC: u64 = 1_000_000;
    /// `av_gettime()` is microseconds since the epoch, never 0; the watch uses
    /// 0 as its "not armed" sentinel, so the tests use a realistic clock.
    const T0: u64 = 1_700_000 * SEC;

    fn at(secs: u64) -> u64 {
        T0 + secs * SEC
    }

    #[test]
    fn drain_watch_fires_after_twenty_seconds() {
        let mut w = DrainWatch::default();
        assert_eq!(w.observe(900, 1.05, 120, at(0)), None);
        // Still under 20 s.
        assert_eq!(w.observe(900, 1.05, 120, at(19)), None);
        let report = w.observe(895, 1.05, 120, at(20)).expect("stuck report");
        assert_eq!(
            report,
            StuckReport {
                start_fill_ms: 900,
                fill_ms: 895,
                stuck_s: 20,
            }
        );
    }

    #[test]
    fn drain_watch_is_silent_below_full_authority() {
        let mut w = DrainWatch::default();
        // Speed below the ceiling.
        assert_eq!(w.observe(900, 1.04, 120, at(0)), None);
        assert_eq!(w.observe(900, 1.04, 120, at(60)), None);
        // At the ceiling but inside the deadband.
        assert_eq!(w.observe(140, 1.05, 120, at(0)), None);
        assert_eq!(w.observe(140, 1.05, 120, at(60)), None);
    }

    #[test]
    fn drain_watch_resets_on_100ms_of_progress() {
        let mut w = DrainWatch::default();
        w.observe(900, 1.05, 120, at(0));
        // 100 ms down at 19 s re-opens the window from there.
        assert_eq!(w.observe(800, 1.05, 120, at(19)), None);
        assert_eq!(w.observe(800, 1.05, 120, at(38)), None);
        assert!(w.observe(800, 1.05, 120, at(39)).is_some());
    }

    #[test]
    fn drain_watch_rearms_once_per_twenty_seconds() {
        let mut w = DrainWatch::default();
        w.observe(900, 1.05, 120, at(0));
        assert!(w.observe(900, 1.05, 120, at(20)).is_some());
        // Silent for the next 20 s even though it is still stuck.
        assert_eq!(w.observe(900, 1.05, 120, at(25)), None);
        assert_eq!(w.observe(900, 1.05, 120, at(39)), None);
        assert!(w.observe(900, 1.05, 120, at(40)).is_some());
    }

    #[test]
    fn drain_watch_recovery_clears_the_window() {
        let mut w = DrainWatch::default();
        w.observe(900, 1.05, 120, at(0));
        // Buffer back inside the deadband: the window closes.
        assert_eq!(w.observe(130, 1.05, 120, at(10)), None);
        // A new stall starts counting from scratch.
        assert_eq!(w.observe(900, 1.05, 120, at(11)), None);
        assert_eq!(w.observe(900, 1.05, 120, at(30)), None);
    }
}
