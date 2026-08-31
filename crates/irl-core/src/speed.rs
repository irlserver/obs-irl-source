//! Playback-speed controller and stuck-drain watch (ports of
//! `compute_buffered_output_speed`, `audio_update_speed_trim`,
//! `apply_output_speed`'s fractional carry and `audio_check_drain_progress`).
//!
//! The controller is PI: a fast proportional ramp with an almost-flat
//! deadband and asymmetric authority, plus a slow integral trim underneath it.
//! `docs/audio-timing-pitfalls.md` is the record of why each piece is shaped
//! the way it is; `cargo run -p irl-core --example speed-controller-sim` is
//! the closed-loop harness that found every defect it ever had.

use crate::config::Watermarks;
use crate::consts;

/// The drain ceiling for a Catch-Up Speed setting, clamped to the slider's
/// range so a scene collection saved by a build with different bounds cannot
/// widen it.
///
/// Read per use rather than cached: the slider applies live, and every
/// consumer (ramp, anti-windup, clamp, stuck-drain detection) has to agree on
/// the same value within a cycle for the anti-windup comparisons to hold.
pub fn catchup_speed_max(catchup_percent: i32) -> f32 {
    let pct = catchup_percent.clamp(consts::CATCHUP_PERCENT_MIN, consts::CATCHUP_PERCENT_MAX);
    1.0 + pct as f32 / 100.0
}

/// Everything the controller reads that is not the buffer level.
#[derive(Debug, Clone, Copy)]
pub struct SpeedInputs {
    /// The live watermarks.
    pub wm: Watermarks,
    /// Adaptive Latency Control.
    pub adaptive: bool,
    /// Low Latency Audio: no speed control at all.
    pub low_latency: bool,
    /// Drain ceiling, from [`catchup_speed_max`].
    pub max_speed: f32,
    /// FFmpeg `av_gettime` microseconds, for the trim's dt.
    pub now_us: u64,
    /// `irl_audio_recovery_active`: concealment or post-reset recovery is
    /// moving the fill for reasons that are not the sender's clock.
    pub recovery_active: bool,
}

/// The integral term that holds the buffer at target when the sender's media
/// clock is not wall clock.
///
/// The ramp in [`SpeedController`] is proportional: it only produces a speed
/// away from 1.0 while the buffer sits away from target. That is the right
/// shape for a transient — a stall's backlog drains and the ramp relaxes —
/// but it cannot hold a *constant*. A sender whose media clock runs at 1.003×
/// delivers 3 ms of extra audio every second forever, and the only ramp
/// position that consumes it is one with a permanent level error, so the
/// buffer parks off-target and the latency parks with it, right up until the
/// offset re-anchor concedes and splices.
///
/// The trim removes that standing error. It accumulates only in the ramp's
/// linear region, where the level genuinely reports the sender's rate, and is
/// clamped to ±1 %. It converges to the sender's rate without ever measuring
/// it — see the estimator section of `docs/audio-timing-pitfalls.md` for the
/// measurement that was built, measured and deleted.
///
/// Lives in the audio state rather than on the pump because its lifetime is
/// not the pump's: it deliberately survives a throttled decoder flush (an
/// audio-only reset must not cost two minutes of relearning) and is cleared by
/// a stream reset, where the timeline broke badly enough that the level no
/// longer maps to the sender's clock and a reconnect may not even be the same
/// encoder.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpeedTrim {
    value: f32,
    last_us: u64,
}

impl SpeedTrim {
    /// A trim that has learned nothing.
    pub const fn new() -> Self {
        Self {
            value: 0.0,
            last_us: 0,
        }
    }

    /// The learned offset from 1.0, in speed units (0.003 is +0.3 %).
    pub fn value(&self) -> f32 {
        self.value
    }

    /// Forget the sender (`irl_reset_stream_timing_state`).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Integrate one cycle's level error. `ramp` is the proportional term of
    /// this cycle, passed in so the anti-windup can tell whether the loop is
    /// still in control.
    fn update(&mut self, fill_ms: i32, target_ms: i32, ramp: f32, inp: &SpeedInputs) {
        let last_us = self.last_us;
        self.last_us = inp.now_us;

        // First cycle after a start or a stall: re-seed dt, integrate nothing.
        if last_us == 0 || inp.now_us <= last_us {
            return;
        }
        let elapsed_us = inp.now_us - last_us;
        if elapsed_us > consts::AUDIO_SPEED_TRIM_MAX_DT_US {
            return;
        }

        // Concealment and post-reset recovery move the fill for reasons that
        // are not the sender's clock. Integrating there would learn the
        // outage.
        if inp.recovery_active {
            return;
        }

        let err_ms = fill_ms - target_ms;
        if err_ms.abs() > consts::AUDIO_SPEED_TRIM_ERR_WINDOW_MS {
            return;
        }

        let dt = elapsed_us as f64 / 1_000_000.0;
        let err_s = err_ms as f64 / 1000.0;
        let step = consts::AUDIO_SPEED_TRIM_GAIN * err_s * dt;

        // Anti-windup, the second half of the error-window gate above.
        //
        // While the loop is saturated the level has stopped reporting the
        // sender's rate — it reports that the controller ran out of authority.
        // At the default target the window gate already covers this, but a
        // small target puts min_ms within 60 ms of target and the command can
        // pin while the error is still inside the window, so the check earns
        // its place here.
        //
        // Test the command actually issued, not the ramp alone: the actuator
        // clamps ramp + trim, so with the trim near its own limit the sum
        // saturates while the ramp is still short of it.
        let command = ramp + self.value;
        let pinned_high = command >= inp.max_speed - 0.0005;
        let pinned_low = command <= consts::AUDIO_SPEED_MIN + 0.0005;
        if (pinned_high && step > 0.0) || (pinned_low && step < 0.0) {
            return;
        }

        let max = consts::AUDIO_SPEED_TRIM_MAX as f64;
        self.value = (self.value as f64 + step).clamp(-max, max) as f32;
    }
}

/// Asymmetric speed controller: builds at up to −2 %, drains at up to the
/// Catch-Up Speed, with a ±20 ms near-flat deadband around target and an EMA
/// of 0.05 per step.
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
    /// at 1.0 immediately and forgets the trim: there is no buffer to
    /// regulate, so a trim learned before the setting changed describes a loop
    /// that no longer runs.
    pub fn update(&mut self, fill_ms: i32, trim: &mut SpeedTrim, inp: SpeedInputs) -> f32 {
        if !inp.adaptive || inp.low_latency {
            self.current = 1.0;
            trim.reset();
            return 1.0;
        }

        let ramp = Self::ramp(fill_ms, inp.wm, inp.max_speed);
        trim.update(fill_ms, inp.wm.target_ms, ramp, &inp);

        // The trim shifts the operating point the ramp swings around; the hard
        // clamp below is unchanged, because the min and the catch-up ceiling
        // are the audibility limits and hold absolutely. That the slow-down
        // authority shrinks to −1 % once the trim has learned +1 % is correct,
        // not a loss: having established that the sender runs fast, dropping
        // to 0.98 absolute would be over-correcting.
        let target_speed = ramp + trim.value();

        if self.current <= 0.0 {
            self.current = 1.0;
        }
        self.current += (target_speed - self.current) * consts::AUDIO_SPEED_SMOOTHING;
        self.current = self.current.clamp(consts::AUDIO_SPEED_MIN, inp.max_speed);
        self.current
    }

    /// The proportional term for one fill level.
    fn ramp(fill_ms: i32, wm: Watermarks, max_speed: f32) -> f32 {
        let low_edge = wm.target_ms - consts::AUDIO_SPEED_DEADBAND_MS;
        let high_edge = wm.target_ms + consts::AUDIO_SPEED_DEADBAND_MS;

        if fill_ms < low_edge {
            let span = low_edge - wm.min_ms;
            let t = if span > 0 {
                ((low_edge - fill_ms) as f32 / span as f32).min(1.0)
            } else {
                1.0
            };
            let edge = 1.0 - consts::AUDIO_SPEED_DEADBAND_SLOPE;
            edge - (edge - consts::AUDIO_SPEED_MIN) * t
        } else if fill_ms > high_edge {
            let span = wm.max_ms - high_edge;
            let t = if span > 0 {
                ((fill_ms - high_edge) as f32 / span as f32).min(1.0)
            } else {
                1.0
            };
            let edge = 1.0 + consts::AUDIO_SPEED_DEADBAND_SLOPE;
            edge + (max_speed - edge) * t
        } else {
            // Shallow slope rather than a flat 1.0: see
            // `AUDIO_SPEED_DEADBAND_SLOPE`. This is what damps the trim.
            1.0 + consts::AUDIO_SPEED_DEADBAND_SLOPE * (fill_ms - wm.target_ms) as f32
                / consts::AUDIO_SPEED_DEADBAND_MS as f32
        }
    }

    /// The smoothed speed.
    pub fn current(&self) -> f32 {
        self.current
    }

    /// Reset to 1.0 (reconnect). The trim is a separate lifetime and is not
    /// touched here.
    pub fn reset(&mut self) {
        self.current = 1.0;
    }
}

impl Default for SpeedController {
    fn default() -> Self {
        Self::new()
    }
}

/// The fractional output-sample debt carried between chunks.
///
/// The resampler is driven in whole samples per chunk, so rounding each chunk
/// independently quantises the applied speed to multiples of `1/in_frames` —
/// about 0.1 % at 1024 frames. Everything the controller asks for below that
/// either rounds away to 1.0 or gets executed at twice its size, which makes
/// both the deadband slope and the trim meaningless, and makes the
/// compensation chatter on and off as the request crosses a rounding boundary.
/// Carrying the remainder makes the long-run rate exact at any requested
/// speed.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpeedCarry {
    frac: f64,
}

impl SpeedCarry {
    /// No debt.
    pub const fn new() -> Self {
        Self { frac: 0.0 }
    }

    /// Forget the debt (`irl_reset_audio_timing_state`).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Output frames to ask swresample for, given an input chunk and a speed.
    pub fn output_frames(&mut self, in_frames: i32, speed: f32) -> i32 {
        let want = in_frames as f64 / speed as f64 + self.frac;
        let desired = ((want + 0.5) as i32).max(1);
        // Bounded so a pathological speed or a clamped `desired` cannot let
        // the debt run away and dump a correction into some later chunk.
        self.frac = (want - desired as f64).clamp(-1.0, 1.0);
        desired
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
/// The drain is bounded by the Catch-Up Speed, so a sender whose media clock
/// runs faster than that can never be caught up with: the buffer rises to the
/// read loop's bleed ceiling and parks there. Nothing the plugin may do fixes
/// that, but it should not look like normal operation either.
///
/// Twenty seconds sits clear of a legitimate burst: even a backlog filling the
/// ceiling drains back under `buffer_max` in about 13 s at the default target
/// and catch-up speed, after which the ramp backs off on its own. A lower
/// catch-up setting drains slower, but "slower" is still progress, which is
/// what [`DrainWatch::observe`] actually tests for.
#[derive(Debug, Clone, Default)]
pub struct DrainWatch {
    since_us: u64,
    fill_ms: i32,
    warn_time_us: u64,
}

impl DrainWatch {
    /// Observe one cycle. Returns a report when the warning should be logged
    /// (at most once per 20 s). `max_speed` is the same drain ceiling the
    /// controller was given this cycle.
    pub fn observe(
        &mut self,
        fill_ms: i32,
        speed: f32,
        target_ms: i32,
        max_speed: f32,
        now_us: u64,
    ) -> Option<StuckReport> {
        let at_full_authority =
            speed >= max_speed - 0.0005 && fill_ms > target_ms + consts::AUDIO_SPEED_DEADBAND_MS;

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

    /// The +5 % default ceiling, which most of these tests run at.
    const MAX: f32 = 1.05;

    /// `av_gettime()` is microseconds since the epoch, never 0; the trim and
    /// the watch both use 0 as a "not armed" sentinel, so the tests use a
    /// realistic clock.
    const SEC: u64 = 1_000_000;
    const T0: u64 = 1_700_000 * SEC;

    fn at(secs: u64) -> u64 {
        T0 + secs * SEC
    }

    fn inputs(now_us: u64) -> SpeedInputs {
        SpeedInputs {
            wm: WM,
            adaptive: true,
            low_latency: false,
            max_speed: MAX,
            now_us,
            recovery_active: false,
        }
    }

    /// Run the controller until it settles, so a test can look at the target
    /// the EMA is converging on rather than one 5 % step towards it.
    ///
    /// The clock is frozen, so dt is zero and the trim never accumulates: this
    /// is the proportional ramp alone. The trim has its own tests below.
    fn settle(fill_ms: i32) -> f32 {
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        let mut speed = 1.0;
        for _ in 0..2000 {
            speed = c.update(fill_ms, &mut trim, inputs(at(0)));
        }
        assert_eq!(trim.value(), 0.0, "frozen clock must not integrate");
        speed
    }

    #[test]
    fn deadband_is_a_shallow_slope_not_a_flat_line() {
        // Dead on target is exactly 1.0; either side of it slopes to the
        // 0.2 % edge value, which is what damps the trim.
        assert_eq!(settle(120), 1.0);
        assert!((settle(140) - (1.0 + consts::AUDIO_SPEED_DEADBAND_SLOPE)).abs() < 1e-4);
        assert!((settle(100) - (1.0 - consts::AUDIO_SPEED_DEADBAND_SLOPE)).abs() < 1e-4);
        // Half a deadband out is half the slope.
        assert!((settle(130) - (1.0 + consts::AUDIO_SPEED_DEADBAND_SLOPE / 2.0)).abs() < 1e-4);
    }

    #[test]
    fn the_ramp_is_continuous_across_the_deadband_edges() {
        // The old flat deadband stepped here. One millisecond either side of
        // an edge must now differ by about one millisecond's worth of slope.
        for edge in [100, 140] {
            let inside = settle(edge);
            let outside = settle(if edge == 100 { edge - 1 } else { edge + 1 });
            assert!(
                (inside - outside).abs() < 1e-3,
                "step at {edge}: {inside} vs {outside}"
            );
        }
    }

    #[test]
    fn full_authority_at_the_watermarks() {
        // At (and below) buffer_min the build runs at the -2 % floor.
        assert!((settle(WM.min_ms) - consts::AUDIO_SPEED_MIN).abs() < 1e-4);
        assert!((settle(0) - consts::AUDIO_SPEED_MIN).abs() < 1e-4);
        // At (and above) buffer_max the drain runs at the catch-up ceiling.
        assert!((settle(WM.max_ms) - MAX).abs() < 1e-4);
        assert!((settle(5000) - MAX).abs() < 1e-4);
    }

    #[test]
    fn speed_is_clamped_to_the_authority_band() {
        for fill in [-1000, 0, 60, 120, 320, 100_000] {
            let s = settle(fill);
            assert!(
                (consts::AUDIO_SPEED_MIN..=MAX).contains(&s),
                "fill {fill} gave {s}"
            );
        }
    }

    #[test]
    fn ema_moves_five_percent_of_the_way_per_step() {
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        // A full-authority drain target of 1.05 from 1.0: the first step is
        // 5 % of the 0.05 distance.
        let first = c.update(WM.max_ms, &mut trim, inputs(at(0)));
        assert!((first - (1.0 + 0.05 * 0.05)).abs() < 1e-6, "got {first}");
        let second = c.update(WM.max_ms, &mut trim, inputs(at(0)));
        assert!((second - (first + (1.05 - first) * 0.05)).abs() < 1e-6);
    }

    #[test]
    fn ramp_is_linear_between_deadband_and_watermark() {
        // Halfway between the high edge (140) and buffer_max (320) is halfway
        // between the 0.2 % edge value and the +5 % ceiling.
        let mid = settle(140 + (320 - 140) / 2);
        let edge = 1.0 + consts::AUDIO_SPEED_DEADBAND_SLOPE;
        assert!(
            (mid - (edge + (MAX - edge) / 2.0)).abs() < 1e-3,
            "got {mid}"
        );
    }

    #[test]
    fn adaptive_off_pins_speed_to_one_and_forgets_the_trim() {
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        // Learn something first.
        for s in 0..400 {
            c.update(140, &mut trim, inputs(at(s)));
        }
        assert!(trim.value() > 0.0);

        let off = SpeedInputs {
            adaptive: false,
            ..inputs(at(400))
        };
        assert_eq!(c.update(WM.max_ms, &mut trim, off), 1.0);
        assert_eq!(c.current(), 1.0);
        assert_eq!(trim.value(), 0.0);
    }

    #[test]
    fn low_latency_pins_speed_to_one() {
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        c.update(0, &mut trim, inputs(at(0)));
        assert!(c.current() < 1.0);
        let ll = SpeedInputs {
            low_latency: true,
            ..inputs(at(1))
        };
        assert_eq!(c.update(0, &mut trim, ll), 1.0);
        assert_eq!(c.current(), 1.0);
    }

    #[test]
    fn reset_returns_to_one_but_keeps_the_trim() {
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        for s in 0..400 {
            c.update(140, &mut trim, inputs(at(s)));
        }
        let learned = trim.value();
        assert!(learned > 0.0);
        c.reset();
        assert_eq!(c.current(), 1.0);
        // A decoder flush must not cost two minutes of relearning.
        assert_eq!(trim.value(), learned);
        trim.reset();
        assert_eq!(trim.value(), 0.0);
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
        let mut trim = SpeedTrim::new();
        let inp = SpeedInputs {
            wm,
            ..inputs(at(0))
        };
        let mut low = 1.0;
        let mut high = 1.0;
        for _ in 0..2000 {
            low = c.update(0, &mut trim, inp);
        }
        assert!((low - consts::AUDIO_SPEED_MIN).abs() < 1e-4);
        c.reset();
        for _ in 0..2000 {
            high = c.update(1000, &mut trim, inp);
        }
        assert!((high - MAX).abs() < 1e-4);
    }

    // ── catchup_speed_max ──

    #[test]
    fn catchup_percent_maps_to_a_speed_and_clamps() {
        assert!((catchup_speed_max(5) - 1.05).abs() < 1e-6);
        assert!((catchup_speed_max(2) - 1.02).abs() < 1e-6);
        assert!((catchup_speed_max(15) - 1.15).abs() < 1e-6);
        // Anything a scene collection can carry lands inside the slider.
        assert!((catchup_speed_max(0) - 1.02).abs() < 1e-6);
        assert!((catchup_speed_max(-100) - 1.02).abs() < 1e-6);
        assert!((catchup_speed_max(1000) - 1.15).abs() < 1e-6);
    }

    #[test]
    fn the_catchup_setting_moves_the_drain_ceiling() {
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        let inp = SpeedInputs {
            max_speed: catchup_speed_max(2),
            ..inputs(at(0))
        };
        let mut speed = 1.0;
        for _ in 0..2000 {
            speed = c.update(WM.max_ms, &mut trim, inp);
        }
        assert!((speed - 1.02).abs() < 1e-4, "got {speed}");
    }

    // ── SpeedTrim ──

    /// Drive the loop against a sender delivering `rate` seconds of media per
    /// second of wall clock, with a continuous buffer level. One step is one
    /// 1024-frame chunk at 48 kHz.
    fn simulate(rate: f64, secs: f64, trim_enabled: bool) -> (f64, f32) {
        const DT_US: u64 = 21_333;
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        let mut fill = WM.target_ms as f64;
        let steps = (secs * 1_000_000.0 / DT_US as f64) as u64;
        for i in 0..steps {
            let inp = inputs(T0 + i * DT_US);
            let speed = c.update(fill as i32, &mut trim, inp);
            if !trim_enabled {
                trim.reset();
            }
            fill += (rate - speed as f64) * (DT_US as f64 / 1_000_000.0) * 1000.0;
            fill = fill.max(0.0);
        }
        (fill, trim.value())
    }

    #[test]
    fn a_proportional_loop_alone_parks_off_target() {
        // The defect the trim exists to fix: a sender 0.3 % fast leaves the
        // ramp holding a permanent level error, and latency parks with it.
        let (fill, _) = simulate(1.003, 300.0, false);
        assert!(
            fill - WM.target_ms as f64 > 20.0,
            "proportional-only should park well high, got {fill}"
        );
    }

    #[test]
    fn the_trim_removes_the_standing_error() {
        for rate in [1.0001, 1.003, 0.997] {
            let (fill, learned) = simulate(rate, 300.0, true);
            let err = fill - WM.target_ms as f64;
            assert!(err.abs() < 5.0, "rate {rate} parked at {fill} ({err:+})");
            // It converged on the sender's rate without measuring it.
            let want = rate - 1.0;
            assert!(
                (learned as f64 - want).abs() < 5e-4,
                "rate {rate} learned {learned}, wanted ~{want}"
            );
        }
    }

    #[test]
    fn the_loop_settles_rather_than_limit_cycling() {
        // The flat deadband left the integrator undamped and the pair swung
        // ±20 ms forever. Measure the swing after it has had time to settle.
        const DT_US: u64 = 21_333;
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        let mut fill = WM.target_ms as f64;
        let step = |c: &mut SpeedController, trim: &mut SpeedTrim, i: u64, fill: &mut f64| {
            let speed = c.update(*fill as i32, trim, inputs(T0 + i * DT_US));
            *fill += (1.003 - speed as f64) * (DT_US as f64 / 1_000_000.0) * 1000.0;
        };
        for i in 0..(300_000_000 / DT_US) {
            step(&mut c, &mut trim, i, &mut fill);
        }
        let (mut peak, mut trough) = (fill, fill);
        let settled_at = 300_000_000 / DT_US;
        for i in settled_at..(settled_at + 100_000_000 / DT_US) {
            step(&mut c, &mut trim, i, &mut fill);
            peak = peak.max(fill);
            trough = trough.min(fill);
        }
        assert!(peak - trough < 5.0, "swing {} ms", peak - trough);
    }

    #[test]
    fn a_network_stall_teaches_the_trim_nothing() {
        // The failure mode that makes naive integrators unusable: a 3 s
        // outage followed by its whole backlog landing at once is a network
        // event, not a clock, and must not move the trim.
        const DT_US: u64 = 21_333;
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        let mut fill = WM.target_ms as f64;
        let mut t = 0u64;

        let run = |c: &mut SpeedController,
                   trim: &mut SpeedTrim,
                   fill: &mut f64,
                   t: &mut u64,
                   rate: f64,
                   secs: f64,
                   recovery: bool| {
            for _ in 0..((secs * 1_000_000.0 / DT_US as f64) as u64) {
                let inp = SpeedInputs {
                    recovery_active: recovery,
                    ..inputs(T0 + *t * DT_US)
                };
                let speed = c.update(*fill as i32, trim, inp);
                *fill = (*fill + (rate - speed as f64) * (DT_US as f64 / 1_000_000.0) * 1000.0)
                    .max(0.0);
                *t += 1;
            }
        };

        run(&mut c, &mut trim, &mut fill, &mut t, 1.0, 120.0, false);
        let before = trim.value();
        // Nothing arriving; the pump is concealing.
        run(&mut c, &mut trim, &mut fill, &mut t, 0.0, 3.0, true);
        fill += 3000.0; // the backlog lands
        run(&mut c, &mut trim, &mut fill, &mut t, 1.0, 300.0, false);

        assert!(
            trim.value().abs() < 1e-5,
            "stall leaked into the trim: {before} -> {}",
            trim.value()
        );
    }

    #[test]
    fn the_trim_ignores_a_gap_where_the_thread_was_not_running() {
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        c.update(180, &mut trim, inputs(at(0)));
        // A dt over the cap (a debugger, a laptop sleep) credits nothing.
        c.update(180, &mut trim, inputs(at(5)));
        assert_eq!(trim.value(), 0.0);
        // A backwards clock is likewise only a re-seed.
        c.update(180, &mut trim, inputs(at(4)));
        assert_eq!(trim.value(), 0.0);
    }

    #[test]
    fn the_trim_only_integrates_near_target() {
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        // 61 ms out is outside the ±60 ms window.
        for s in 0..400 {
            c.update(WM.target_ms + 61, &mut trim, inputs(at(s)));
        }
        assert_eq!(trim.value(), 0.0);
        // 59 ms out is inside it.
        for s in 400..800 {
            c.update(WM.target_ms + 59, &mut trim, inputs(at(s)));
        }
        assert!(trim.value() > 0.0);
    }

    #[test]
    fn anti_windup_tests_the_command_not_the_ramp() {
        // A small target puts min_ms within the error window, so the command
        // can pin while the level is still inside it and the ramp alone is
        // still short of the limit. The trim must stop where the *sum* pins,
        // not where the ramp would.
        let wm = Watermarks::derive(60);
        let fill = 35;
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        let inp = SpeedInputs {
            wm,
            ..inputs(at(0))
        };
        for s in 0..200_000 {
            c.update(
                fill,
                &mut trim,
                SpeedInputs {
                    now_us: at(s),
                    ..inp
                },
            );
        }

        let ramp = SpeedController::ramp(fill, wm, inp.max_speed);
        assert!(
            ramp > consts::AUDIO_SPEED_MIN + 0.0005,
            "ramp alone should not pin, got {ramp}"
        );
        // It stopped at the pin, before its own ±1 % clamp would have.
        assert!(
            trim.value() > -consts::AUDIO_SPEED_TRIM_MAX,
            "the clamp bound first, so this tests nothing: {}",
            trim.value()
        );
        assert!(
            ramp + trim.value() >= consts::AUDIO_SPEED_MIN - 1e-4,
            "command wound past the floor: {}",
            ramp + trim.value()
        );
    }

    #[test]
    fn the_trim_is_clamped_to_one_percent() {
        let mut c = SpeedController::new();
        let mut trim = SpeedTrim::new();
        // Sit just inside the window, forever, at a ceiling wide enough that
        // the command never pins.
        let inp = SpeedInputs {
            max_speed: catchup_speed_max(15),
            ..inputs(at(0))
        };
        for s in 0..200_000 {
            c.update(
                WM.target_ms + 59,
                &mut trim,
                SpeedInputs {
                    now_us: at(s),
                    ..inp
                },
            );
        }
        assert!((trim.value() - consts::AUDIO_SPEED_TRIM_MAX).abs() < 1e-6);
    }

    // ── SpeedCarry ──

    /// The speed actually realised over `chunks` chunks of `n` frames.
    fn applied_speed(requested: f32, n: i32, chunks: i32) -> f64 {
        let mut carry = SpeedCarry::new();
        let mut out = 0i64;
        for _ in 0..chunks {
            out += carry.output_frames(n, requested) as i64;
        }
        (n as i64 * chunks as i64) as f64 / out as f64
    }

    #[test]
    fn the_carry_applies_speeds_below_the_rounding_step() {
        // Without it, 1/1024 (~0.1 %) is the smallest applicable speed: a
        // requested +0.02 % was discarded and +0.05 % came out at +0.098 %.
        // That is the whole range the deadband slope and most of the trim
        // operate in.
        for req in [
            1.0f32, 1.0002, 1.0005, 1.001, 1.002, 1.005, 1.01, 0.9995, 0.998, 0.99,
        ] {
            let got = applied_speed(req, 1024, 4000);
            let err = (got - req as f64) * 100.0;
            assert!(err.abs() < 0.005, "req {req} applied {got} (err {err:+}%)");
        }
    }

    #[test]
    fn naive_rounding_would_fail_that_test() {
        // Pin the defect the carry fixes, so a "simplification" that drops it
        // does not pass silently.
        let n = 1024;
        let naive = (n as f32 / 1.0002 + 0.5) as i32;
        assert_eq!(naive, n, "rounding alone discards +0.02 %");
    }

    #[test]
    fn the_carry_never_asks_for_less_than_one_frame() {
        let mut carry = SpeedCarry::new();
        assert_eq!(carry.output_frames(1, 1000.0), 1);
        assert_eq!(carry.output_frames(0, 1.0), 1);
    }

    #[test]
    fn reset_forgets_the_debt() {
        let mut carry = SpeedCarry::new();
        carry.output_frames(1024, 1.0005);
        carry.reset();
        assert_eq!(carry.output_frames(1024, 1.0), 1024);
    }

    // ── DrainWatch ──

    #[test]
    fn drain_watch_fires_after_twenty_seconds() {
        let mut w = DrainWatch::default();
        assert_eq!(w.observe(900, 1.05, 120, MAX, at(0)), None);
        // Still under 20 s.
        assert_eq!(w.observe(900, 1.05, 120, MAX, at(19)), None);
        let report = w
            .observe(895, 1.05, 120, MAX, at(20))
            .expect("stuck report");
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
        assert_eq!(w.observe(900, 1.04, 120, MAX, at(0)), None);
        assert_eq!(w.observe(900, 1.04, 120, MAX, at(60)), None);
        // At the ceiling but inside the deadband.
        assert_eq!(w.observe(140, 1.05, 120, MAX, at(0)), None);
        assert_eq!(w.observe(140, 1.05, 120, MAX, at(60)), None);
    }

    #[test]
    fn drain_watch_follows_the_catchup_setting() {
        // +4 % is full authority when the user asked for 4 %, and is not when
        // they asked for 5 %.
        let mut w = DrainWatch::default();
        let max = catchup_speed_max(4);
        w.observe(900, 1.04, 120, max, at(0));
        assert!(w.observe(900, 1.04, 120, max, at(20)).is_some());

        let mut w = DrainWatch::default();
        w.observe(900, 1.04, 120, MAX, at(0));
        assert_eq!(w.observe(900, 1.04, 120, MAX, at(20)), None);
    }

    #[test]
    fn drain_watch_resets_on_100ms_of_progress() {
        let mut w = DrainWatch::default();
        w.observe(900, 1.05, 120, MAX, at(0));
        // 100 ms down at 19 s re-opens the window from there.
        assert_eq!(w.observe(800, 1.05, 120, MAX, at(19)), None);
        assert_eq!(w.observe(800, 1.05, 120, MAX, at(38)), None);
        assert!(w.observe(800, 1.05, 120, MAX, at(39)).is_some());
    }

    #[test]
    fn drain_watch_rearms_once_per_twenty_seconds() {
        let mut w = DrainWatch::default();
        w.observe(900, 1.05, 120, MAX, at(0));
        assert!(w.observe(900, 1.05, 120, MAX, at(20)).is_some());
        // Silent for the next 20 s even though it is still stuck.
        assert_eq!(w.observe(900, 1.05, 120, MAX, at(25)), None);
        assert_eq!(w.observe(900, 1.05, 120, MAX, at(39)), None);
        assert!(w.observe(900, 1.05, 120, MAX, at(40)).is_some());
    }

    #[test]
    fn drain_watch_recovery_clears_the_window() {
        let mut w = DrainWatch::default();
        w.observe(900, 1.05, 120, MAX, at(0));
        // Buffer back inside the deadband: the window closes.
        assert_eq!(w.observe(130, 1.05, 120, MAX, at(10)), None);
        // A new stall starts counting from scratch.
        assert_eq!(w.observe(900, 1.05, 120, MAX, at(11)), None);
        assert_eq!(w.observe(900, 1.05, 120, MAX, at(30)), None);
    }
}
