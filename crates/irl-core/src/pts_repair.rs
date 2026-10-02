//! Three-tier PTS discontinuity repair.

use crate::consts;
use crate::rescale;

/// What the caller should do with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtsAction {
    /// Use the PTS as is.
    Pass,
    /// PTS was repaired by interpolation.
    Interpolate,
    /// Insert `silence_ms` of silence before the frame.
    Silence,
    /// Large gap: full timeline reset.
    Reset,
}

/// Result of one evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// Action.
    pub action: PtsAction,
    /// The PTS to use (may differ from the input).
    pub corrected_pts: i64,
    /// Silence to insert (only for `Silence`).
    pub silence_ms: i32,
    /// Magnitude of the gap observed, in milliseconds, for both directions
    /// (the action carries the direction: a backward jump passes or resets).
    pub gap_ms: i32,
}

/// Per-stream repair state.
#[derive(Debug, Clone)]
pub struct PtsRepair {
    last_pts: i64,
    last_duration: i64,
    tb_num: i32,
    tb_den: i32,
    small_gap_ms: i32,
    large_gap_ms: i32,
    last_gap_ms: i32,
    last_action_gap_ms: i32,
    consecutive_small_repairs: i32,
    relocking: bool,
    initialised: bool,
}

impl PtsRepair {
    /// Repair state for one stream; gaps are in milliseconds, the time base
    /// is the stream's.
    pub fn new(small_gap_ms: i32, large_gap_ms: i32, tb_num: i32, tb_den: i32) -> Self {
        Self {
            last_pts: 0,
            last_duration: 0,
            tb_num,
            tb_den,
            small_gap_ms,
            large_gap_ms,
            last_gap_ms: 0,
            last_action_gap_ms: 0,
            consecutive_small_repairs: 0,
            relocking: false,
            initialised: false,
        }
    }

    /// Forget the timeline; the next frame starts a new one.
    pub fn reset(&mut self) {
        self.last_pts = 0;
        self.last_duration = 0;
        self.last_gap_ms = 0;
        self.last_action_gap_ms = 0;
        self.consecutive_small_repairs = 0;
        self.relocking = false;
        self.initialised = false;
    }

    /// Judge one frame's `pts` and `duration` (stream time base).
    pub fn evaluate(&mut self, pts: i64, duration: i64) -> Verdict {
        // First frame — just record and pass through.
        if !self.initialised {
            self.last_pts = pts;
            self.last_duration = if duration > 0 { duration } else { 1 };
            self.last_gap_ms = 0;
            self.consecutive_small_repairs = 0;
            self.relocking = false;
            self.initialised = true;
            return self.verdict(PtsAction::Pass, pts, 0);
        }

        let expected = self.last_pts + self.last_duration;
        let gap = pts - expected;

        let gap_ms = self.ts_to_ms(gap.abs());
        let is_backward = gap < 0;
        self.last_action_gap_ms = gap_ms;

        // Essentially aligned, in either direction: under a millisecond, or
        // within one tick of the stream's own time base. Re-baseline on the
        // PTS the sender actually sent.
        //
        // The tick test makes a frame duration the container cannot express
        // harmless: 1024 samples at 44.1 kHz is 2089.8 ticks of a 90 kHz
        // clock against a carried 2090, so the stream lands one tick early
        // every few frames. Without it that wobble falls into the leading-edge
        // rule below, which freezes the baseline, and the repaired timeline
        // ends up a frame short for good: a standing ~23 ms lip-sync error on
        // every 44.1 kHz stream.
        if gap_ms < 1 || gap.abs() <= 1 {
            self.last_pts = pts;
            self.last_duration = if duration > 0 {
                duration
            } else {
                self.last_duration
            };
            self.last_gap_ms = 0;
            self.consecutive_small_repairs = 0;
            self.relocking = false;
            return self.verdict(PtsAction::Pass, pts, 0);
        }

        if is_backward {
            // Larger backward jumps look like B-frame reorder or decoder ts
            // wobble. Pass through without updating last_pts so the baseline
            // keeps tracking the leading edge of the stream.
            if gap_ms < self.small_gap_ms {
                return self.verdict(PtsAction::Pass, pts, 0);
            }
            // Large backward jump — timeline reset (new segment, sender pts
            // wrap, or remap). Treat exactly like a large forward gap.
            self.last_pts = pts;
            self.last_duration = if duration > 0 {
                duration
            } else {
                self.last_duration
            };
            self.last_gap_ms = 0;
            self.last_action_gap_ms = gap_ms;
            self.consecutive_small_repairs = 0;
            self.relocking = false;
            return self.verdict(PtsAction::Reset, pts, 0);
        }

        let action;
        let corrected_pts;
        let mut silence_ms = 0;

        if gap_ms < self.small_gap_ms {
            let relock_step_ts = self.ms_to_ts_ceil(consts::PTS_RELOCK_STEP_MS);
            if !self.relocking {
                let same_small_gap = self.consecutive_small_repairs > 0
                    && gap_ms >= self.last_gap_ms - consts::PTS_SMALL_GAP_TOLERANCE_MS
                    && gap_ms <= self.last_gap_ms + consts::PTS_SMALL_GAP_TOLERANCE_MS;
                if same_small_gap {
                    self.consecutive_small_repairs += 1;
                } else {
                    self.consecutive_small_repairs = 1;
                }
                self.last_gap_ms = gap_ms;

                // If the same small positive gap repeats for long enough,
                // corruption likely shifted the sender timeline and the old
                // baseline is now wrong. Enter a short relock phase and slew
                // toward the new baseline instead of snapping.
                if self.consecutive_small_repairs >= consts::PTS_SMALL_GAP_RELOCK_COUNT {
                    self.relocking = true;
                    self.last_gap_ms = 0;
                    self.consecutive_small_repairs = 0;
                }
            }

            if self.relocking {
                if gap <= relock_step_ts {
                    self.last_pts = pts;
                    self.last_duration = if duration > 0 {
                        duration
                    } else {
                        self.last_duration
                    };
                    self.last_gap_ms = 0;
                    self.consecutive_small_repairs = 0;
                    self.relocking = false;
                    return self.verdict(PtsAction::Pass, pts, 0);
                }
                corrected_pts = expected + relock_step_ts;
                action = PtsAction::Interpolate;
            } else {
                // Small gap — interpolate: use expected PTS.
                corrected_pts = expected;
                action = PtsAction::Interpolate;
            }
        } else if gap_ms < self.large_gap_ms {
            self.last_gap_ms = 0;
            self.consecutive_small_repairs = 0;
            self.relocking = false;
            // Medium gap — insert silence, then use the original PTS.
            corrected_pts = pts;
            silence_ms = gap_ms;
            action = PtsAction::Silence;
        } else {
            self.last_gap_ms = 0;
            self.consecutive_small_repairs = 0;
            self.relocking = false;
            // Large gap — full reset.
            corrected_pts = pts;
            action = PtsAction::Reset;
        }

        self.last_pts = corrected_pts;
        self.last_duration = if duration > 0 {
            duration
        } else {
            self.last_duration
        };

        self.verdict(action, corrected_pts, silence_ms)
    }

    /// Last known-good PTS and duration (stream time base), for extrapolating
    /// a frame that carries no timestamp. `None` before the first frame.
    pub fn last(&self) -> Option<(i64, i64)> {
        if !self.initialised {
            return None;
        }
        Some((self.last_pts, self.last_duration))
    }

    /// The stream time base the repair state was built with.
    pub fn time_base(&self) -> (i32, i32) {
        (self.tb_num, self.tb_den)
    }

    /// Gap of the most recent evaluation, in milliseconds.
    pub fn last_action_gap_ms(&self) -> i32 {
        self.last_action_gap_ms
    }

    // ── internals ──

    fn verdict(&self, action: PtsAction, corrected_pts: i64, silence_ms: i32) -> Verdict {
        Verdict {
            action,
            corrected_pts,
            silence_ms,
            gap_ms: self.last_action_gap_ms,
        }
    }

    fn ts_to_ms(&self, ts: i64) -> i32 {
        if self.tb_den <= 0 || self.tb_num <= 0 {
            return 0;
        }
        let ms = rescale::rescale_q_near(ts, self.tb_num as i64, self.tb_den as i64, 1, 1000);
        ms.clamp(i32::MIN as i64, i32::MAX as i64) as i32
    }

    fn ms_to_ts_ceil(&self, ms: i32) -> i64 {
        if self.tb_num <= 0 || self.tb_den <= 0 || ms <= 0 {
            return 1;
        }
        let ts = rescale::rescale_q_up(ms as i64, 1, 1000, self.tb_num as i64, self.tb_den as i64);
        if ts > 0 { ts } else { 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 48 kHz audio time base: 1 tick = 1 sample, 48 ticks = 1 ms.
    const TB_NUM: i32 = 1;
    const TB_DEN: i32 = 48_000;
    /// One 20 ms packet.
    const DUR: i64 = 960;

    fn repair() -> PtsRepair {
        PtsRepair::new(consts::SMALL_GAP_MS, consts::LARGE_GAP_MS, TB_NUM, TB_DEN)
    }

    /// 1024 samples at 44.1 kHz is 2089.795 ticks of a 90 kHz clock, so the
    /// duration the container carries is 2090 and the stream lands one tick
    /// early every few frames. The repaired timeline must track the sender,
    /// not lose a frame to it: the audio→video playout mapping is derived from
    /// these PTS, so a frame lost here is a standing ~23 ms lip-sync error on
    /// every 44.1 kHz stream. 48 kHz divides the 90 kHz clock exactly.
    #[test]
    fn a_frame_duration_the_time_base_cannot_express_does_not_lose_a_frame() {
        for (rate, tb_den) in [(44_100i64, 90_000i64), (44_100, 1_000), (48_000, 90_000)] {
            let mut repair =
                PtsRepair::new(consts::SMALL_GAP_MS, consts::LARGE_GAP_MS, 1, tb_den as i32);
            let duration = (1024 * tb_den + rate / 2) / rate;
            for chunk in 0..2_000i64 {
                let pts = (chunk * 1024 * tb_den + rate / 2) / rate;
                let verdict = repair.evaluate(pts, duration);
                assert_eq!(
                    verdict.action,
                    PtsAction::Pass,
                    "rate={rate} tb=1/{tb_den} chunk={chunk}: a rounding wobble is not a gap"
                );
                assert_eq!(verdict.corrected_pts, pts, "rate={rate} tb=1/{tb_den}");
            }
        }
    }

    /// The leading-edge rule still holds for a backward jump big enough to be
    /// a real reorder: the baseline stays on the leading frame, so the frame
    /// that follows it is not read as a gap.
    #[test]
    fn a_reorder_sized_backward_jump_still_leaves_the_baseline_alone() {
        let mut repair = repair();
        repair.evaluate(0, DUR);
        repair.evaluate(DUR, DUR);
        // The stream jumps a frame ahead, which the small-gap path repairs on
        // to the expected PTS, and then delivers the frame it skipped: 20 ms
        // back, far past a tick of rounding and well inside the small gap.
        assert_eq!(repair.evaluate(3 * DUR, DUR).action, PtsAction::Interpolate);
        let verdict = repair.evaluate(2 * DUR, DUR);
        assert_eq!(verdict.action, PtsAction::Pass);
        assert_eq!(verdict.corrected_pts, 2 * DUR);
        // The baseline never left the leading edge, so the next frame in order
        // is not a gap either.
        assert_eq!(repair.evaluate(3 * DUR, DUR).action, PtsAction::Pass);
        assert_eq!(repair.last(), Some((3 * DUR, DUR)));
    }

    fn ms(v: i64) -> i64 {
        v * 48
    }

    #[test]
    fn first_frame_passes() {
        let mut r = repair();
        assert_eq!(r.last(), None);
        let v = r.evaluate(12_345, DUR);
        assert_eq!(v.action, PtsAction::Pass);
        assert_eq!(v.corrected_pts, 12_345);
        assert_eq!(v.gap_ms, 0);
        assert_eq!(v.silence_ms, 0);
        assert_eq!(r.last(), Some((12_345, DUR)));
    }

    /// The three tiers and their edges: under 70 ms is interpolated onto the
    /// expected PTS, 70 ms up to 2 s gets that much silence, and 2 s or more
    /// resets the timeline.
    #[test]
    fn forward_gaps_take_the_tier_their_size_falls_in() {
        let table = [
            (0, PtsAction::Pass, 0),
            (69, PtsAction::Interpolate, 0),
            (70, PtsAction::Silence, 70),
            (1999, PtsAction::Silence, 1999),
            (2000, PtsAction::Reset, 0),
        ];
        for (gap, action, silence_ms) in table {
            let mut r = repair();
            r.evaluate(0, DUR);
            let pts = DUR + ms(i64::from(gap));
            let v = r.evaluate(pts, DUR);
            assert_eq!(v.action, action, "{gap}ms");
            assert_eq!(v.gap_ms, gap, "{gap}ms");
            assert_eq!(v.silence_ms, silence_ms, "{gap}ms");
            // Interpolation snaps to the expected PTS, discarding the gap;
            // every other tier keeps the PTS the stream sent.
            let corrected = if action == PtsAction::Interpolate {
                DUR
            } else {
                pts
            };
            assert_eq!(v.corrected_pts, corrected, "{gap}ms");
            assert_eq!(r.last(), Some((corrected, DUR)), "{gap}ms");
        }
    }

    #[test]
    fn small_backward_jump_passes_without_moving_the_baseline() {
        let mut r = repair();
        r.evaluate(0, DUR);
        let v = r.evaluate(DUR - ms(30), DUR);
        assert_eq!(v.action, PtsAction::Pass);
        assert_eq!(v.gap_ms, 30);
        // The baseline still tracks the leading edge, so the next in-order
        // frame is contiguous rather than a huge forward gap.
        assert_eq!(r.last(), Some((0, DUR)));
        let v = r.evaluate(DUR, DUR);
        assert_eq!(v.action, PtsAction::Pass);
    }

    #[test]
    fn large_backward_jump_resets() {
        let mut r = repair();
        r.evaluate(ms(10_000), DUR);
        let v = r.evaluate(0, DUR);
        assert_eq!(v.action, PtsAction::Reset);
        assert_eq!(v.gap_ms, 10_020);
        assert_eq!(v.corrected_pts, 0);
        assert_eq!(r.last(), Some((0, DUR)));
    }

    /// A sender whose timeline is a constant `offset_ms` ahead of ours: the
    /// input advances by exactly one packet duration per frame, so once
    /// interpolation snaps the baseline back the same gap repeats forever.
    /// That repetition is what relock exists to break.
    fn feed(r: &mut PtsRepair, frame: i64, offset_ms: i64) -> Verdict {
        r.evaluate(frame * DUR + ms(offset_ms), DUR)
    }

    #[test]
    fn eight_identical_small_gaps_enter_relock() {
        let mut r = repair();
        r.evaluate(0, DUR);

        for frame in 1..=7 {
            let expected = r.last().unwrap().0 + DUR;
            let v = feed(&mut r, frame, 30);
            assert_eq!(v.action, PtsAction::Interpolate, "frame {frame}");
            assert_eq!(v.gap_ms, 30);
            assert_eq!(v.corrected_pts, expected, "frame {frame} snaps back");
        }

        // The eighth identical gap flips into relock, which slews by 2 ms
        // instead of discarding the whole gap.
        let expected = r.last().unwrap().0 + DUR;
        let v = feed(&mut r, 8, 30);
        assert_eq!(v.action, PtsAction::Interpolate);
        assert_eq!(v.corrected_pts, expected + ms(2));
    }

    #[test]
    fn relock_slews_two_ms_per_frame() {
        let mut r = repair();
        r.evaluate(0, DUR);
        for frame in 1..=8 {
            feed(&mut r, frame, 30);
        }

        // Still relocking: the baseline advances by one duration plus the
        // 2 ms step, so the residual gap shrinks by 2 ms per frame.
        let mut frame = 9;
        let mut gap_ms = 30;
        while frame < 14 {
            let expected = r.last().unwrap().0 + DUR;
            let v = feed(&mut r, frame, 30);
            assert_eq!(v.action, PtsAction::Interpolate);
            assert_eq!(v.corrected_pts, expected + ms(2));
            assert_eq!(v.gap_ms, gap_ms - 2, "gap shrinks 2 ms per frame");
            gap_ms = v.gap_ms;
            frame += 1;
        }

        // Once the residual is within one step, relock ends on a pass and the
        // frame's own PTS is adopted.
        loop {
            let v = feed(&mut r, frame, 30);
            if v.action == PtsAction::Pass {
                assert_eq!(v.corrected_pts, frame * DUR + ms(30));
                assert!(v.gap_ms <= consts::PTS_RELOCK_STEP_MS);
                break;
            }
            frame += 1;
            assert!(frame < 40, "relock must converge");
        }
    }

    #[test]
    fn relock_counter_tolerates_two_ms_of_wobble() {
        let mut r = repair();
        r.evaluate(0, DUR);

        // Each gap is within ±2 ms of the one before it, so the run holds.
        for (i, w) in [30, 31, 30, 29, 30, 31, 30].into_iter().enumerate() {
            let expected = r.last().unwrap().0 + DUR;
            let v = feed(&mut r, i as i64 + 1, w);
            assert_eq!(v.action, PtsAction::Interpolate);
            assert_eq!(v.corrected_pts, expected);
        }

        // The eighth in the run trips relock.
        let expected = r.last().unwrap().0 + DUR;
        let v = feed(&mut r, 8, 30);
        assert_eq!(v.corrected_pts, expected + ms(2));
    }

    #[test]
    fn gap_outside_tolerance_restarts_the_relock_count() {
        let mut r = repair();
        r.evaluate(0, DUR);

        // Seven at 30 ms, then one at 40 ms (outside ±2 ms) restarts the run,
        // so the next frames are plain interpolations, not relock steps.
        for frame in 1..=7 {
            feed(&mut r, frame, 30);
        }
        let expected = r.last().unwrap().0 + DUR;
        let v = feed(&mut r, 8, 40);
        assert_eq!(v.action, PtsAction::Interpolate);
        assert_eq!(v.corrected_pts, expected);

        let expected = r.last().unwrap().0 + DUR;
        let v = feed(&mut r, 9, 40);
        assert_eq!(v.corrected_pts, expected, "still a plain interpolation");
    }

    #[test]
    fn reset_clears_the_baseline() {
        let mut r = repair();
        r.evaluate(500, DUR);
        r.reset();
        assert_eq!(r.last(), None);
        let v = r.evaluate(9_000, DUR);
        assert_eq!(v.action, PtsAction::Pass);
    }

    #[test]
    fn degenerate_time_base_reports_no_gap() {
        let mut r = PtsRepair::new(consts::SMALL_GAP_MS, consts::LARGE_GAP_MS, 0, 0);
        r.evaluate(0, DUR);
        // ts_to_ms returns 0 for a zero time base, so every forward gap looks
        // sub-millisecond and passes through.
        let v = r.evaluate(1_000_000, DUR);
        assert_eq!(v.action, PtsAction::Pass);
        assert_eq!(v.gap_ms, 0);
        assert_eq!(r.time_base(), (0, 0));
    }
}
